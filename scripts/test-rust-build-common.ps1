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
    "Failed to read response header: connection attempt timed out (os error 10060)"
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

# Execute the CI entrypoint against disposable commands, never the runner's rustup,
# compiler, cache directory, or shared daemon. These fixtures also verify GITHUB_ENV
# clears an inherited wrapper for subsequent steps when cache discovery fails.
$ciSetupPath = Join-Path $PSScriptRoot "ci/setup-rust.ps1"
$ciSetupText = Get-Content -LiteralPath $ciSetupPath -Raw
foreach ($forbiddenDaemonMutation in @("--stop-server", "--start-server", "--zero-stats")) {
    if ($ciSetupText.Contains($forbiddenDaemonMutation, [System.StringComparison]::Ordinal)) {
        throw "Shared sccache daemon mutation is forbidden in CI setup: $forbiddenDaemonMutation"
    }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$workflowText = Get-Content -LiteralPath (Join-Path $repoRoot ".github/workflows/ci.yml") -Raw
$clippyStep = [regex]::Match($workflowText,
    '(?m)^      - name: Strict Windows Clippy without shared cache\r?\n        shell: pwsh\r?\n        run: \|\r?\n(?<commands>(?:          [^\r\n]*\r?\n)+)')
if (-not $clippyStep.Success) { throw "Expected strict Windows Clippy step was not found." }
$clippyCommands = $clippyStep.Groups["commands"].Value -replace '(?m)^          ', ''
$clippyScript = [scriptblock]::Create($clippyCommands)

function Test-AgentBusCiSetupFixture {
    param([Parameter(Mandatory = $true)][string]$Mode)

    $fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("agent-bus-ci-fixture-" + [guid]::NewGuid())
    $environmentNames = @(
        "AGENT_HUB_CI_CACHE_ROOT", "LOCALAPPDATA", "USERPROFILE", "PATH",
        "GITHUB_ENV", "GITHUB_PATH", "GITHUB_JOB", "RUNNER_ARCH",
        "CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "SCCACHE_DIR", "SCCACHE_SERVER_PORT", "RUSTC_WRAPPER"
    )
    $savedEnvironment = @{}
    foreach ($name in $environmentNames) {
        $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
    }
    $exitCodeVariable = Get-Variable -Name LASTEXITCODE -Scope Global -ErrorAction SilentlyContinue
    $hadExitCode = $null -ne $exitCodeVariable
    $savedExitCode = if ($hadExitCode) { $exitCodeVariable.Value } else { $null }
    try {
        New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
        $Mode | Set-Content -LiteralPath (Join-Path $fixtureRoot "mode.txt") -Encoding utf8
        @'
function Write-FixtureInvocation {
    param([string]$Command, [object[]]$Arguments)
    [pscustomobject]@{ command = $Command; arguments = @($Arguments); wrapper = $env:RUSTC_WRAPPER } |
        ConvertTo-Json -Compress | Add-Content -LiteralPath (Join-Path $PSScriptRoot "calls.jsonl") -Encoding utf8
}
'@ | Set-Content -LiteralPath (Join-Path $fixtureRoot "fixture-common.ps1") -Encoding utf8
        @'
. (Join-Path $PSScriptRoot "fixture-common.ps1")
Write-FixtureInvocation -Command "rustup" -Arguments $args
$global:LASTEXITCODE = 0
if ($args[0] -eq "which") {
    Join-Path $PSScriptRoot ($args[1] + ".ps1")
}
'@ | Set-Content -LiteralPath (Join-Path $fixtureRoot "rustup.ps1") -Encoding utf8
        foreach ($compiler in @("cargo", "rustc")) {
            @'
. (Join-Path $PSScriptRoot "fixture-common.ps1")
Write-FixtureInvocation -Command ([IO.Path]::GetFileNameWithoutExtension($PSCommandPath)) -Arguments $args
$global:LASTEXITCODE = 0
$mode = (Get-Content -LiteralPath (Join-Path $PSScriptRoot "mode.txt") -Raw).Trim()
if ($mode -eq "clippy-failure" -and $args -contains "clippy") {
    $global:LASTEXITCODE = 1
    Write-Output "error: fixture strict Clippy warning"
} else {
    Write-Output "fixture compiler 1.98.1"
}
'@ | Set-Content -LiteralPath (Join-Path $fixtureRoot ($compiler + ".ps1")) -Encoding utf8
        }
        @'
. (Join-Path $PSScriptRoot "fixture-common.ps1")
Write-FixtureInvocation -Command "sccache" -Arguments $args
$mode = (Get-Content -LiteralPath (Join-Path $PSScriptRoot "mode.txt") -Raw).Trim()
$global:LASTEXITCODE = 0
switch ($args[0]) {
    "--version" {
        if ($mode -eq "version-failure") {
            $global:LASTEXITCODE = 2
            Write-Output "version unavailable"
        } else {
            Write-Output "sccache 0.16.0"
        }
    }
    "--show-stats" {
        switch ($mode) {
            "mismatched" { Write-Output "Mismatch of client/server versions" }
            "transport-failure" {
                $global:LASTEXITCODE = 1
                Write-Output "Failed to read response header: connection attempt timed out (os error 10060)"
            }
            "probe-exception" { throw "fixture health probe exception" }
            default { Write-Output "Compile requests 0" }
        }
    }
    default { throw "Forbidden or unexpected sccache invocation: $($args -join ' ')" }
}
'@ | Set-Content -LiteralPath (Join-Path $fixtureRoot "sccache.ps1") -Encoding utf8

        $fixtureRustup = Join-Path $fixtureRoot "rustup.ps1"
        $fixtureSccache = Join-Path $fixtureRoot "sccache.ps1"
        function rustup { & $fixtureRustup @args }
        function Get-Command {
            [CmdletBinding()]
            param([string]$Name)
            if ($Name -ne "sccache") { throw "Unexpected command discovery in fixture: $Name" }
            if ($Mode -ne "absent") { [pscustomobject]@{ Source = $fixtureSccache } }
        }

        $env:AGENT_HUB_CI_CACHE_ROOT = Join-Path $fixtureRoot "cache"
        $env:LOCALAPPDATA = Join-Path $fixtureRoot "local"
        $env:USERPROFILE = Join-Path $fixtureRoot "user"
        $env:GITHUB_ENV = Join-Path $fixtureRoot "github-env.txt"
        $env:GITHUB_PATH = Join-Path $fixtureRoot "github-path.txt"
        $env:GITHUB_JOB = "windows fixture"
        $env:RUNNER_ARCH = "X64"
        $env:RUSTC_WRAPPER = "stale-runner-wrapper"
        "RUSTC_WRAPPER=stale-runner-wrapper" | Set-Content -LiteralPath $env:GITHUB_ENV -Encoding utf8
        & $ciSetupPath

        $expectedWrapper = if ($Mode -eq "healthy") { $fixtureSccache } else { $null }
        if ($env:RUSTC_WRAPPER -ne $expectedWrapper) {
            throw "$Mode fixture: unexpected process wrapper '$($env:RUSTC_WRAPPER)'"
        }
        $wrapperEntries = @(Get-Content -LiteralPath $env:GITHUB_ENV | Where-Object { $_ -like "RUSTC_WRAPPER=*" })
        if ($wrapperEntries.Count -ne 2 -or $wrapperEntries[-1] -cne "RUSTC_WRAPPER=$expectedWrapper") {
            throw "$Mode fixture: GITHUB_ENV did not supersede the inherited wrapper"
        }
        $expectedTarget = Join-Path $env:AGENT_HUB_CI_CACHE_ROOT "target/windows_fixture-X64"
        $expectedCache = Join-Path $env:AGENT_HUB_CI_CACHE_ROOT "sccache"
        if ($env:CARGO_TARGET_DIR -ne $expectedTarget -or $env:SCCACHE_DIR -ne $expectedCache -or
            $env:CARGO_INCREMENTAL -ne "0" -or $env:SCCACHE_SERVER_PORT -ne "4228" -or
            -not (Test-Path -LiteralPath $expectedTarget -PathType Container) -or
            -not (Test-Path -LiteralPath $expectedCache -PathType Container)) {
            throw "$Mode fixture: persistent build settings were lost"
        }
        $exportedEnvironment = @(Get-Content -LiteralPath $env:GITHUB_ENV)
        foreach ($requiredEntry in @("CARGO_TARGET_DIR=$expectedTarget", "CARGO_INCREMENTAL=0",
                "SCCACHE_DIR=$expectedCache", "SCCACHE_SERVER_PORT=4228")) {
            if ($exportedEnvironment -cnotcontains $requiredEntry) {
                throw "$Mode fixture: missing GITHUB_ENV entry $requiredEntry"
            }
        }
        $calls = @(Get-Content -LiteralPath (Join-Path $fixtureRoot "calls.jsonl") | ForEach-Object { $_ | ConvertFrom-Json })
        $rustupCalls = @($calls | Where-Object { $_.command -eq "rustup" } | ForEach-Object { $_.arguments -join " " })
        if (($rustupCalls -join "`n") -cne "toolchain install --profile minimal --no-self-update`ncomponent add rustfmt clippy`nwhich cargo`nwhich rustc") {
            throw "$Mode fixture: pinned toolchain/component setup changed"
        }
        $cacheCalls = @($calls | Where-Object { $_.command -eq "sccache" })
        $expectedCacheCalls = switch ($Mode) {
            "absent" { @() }
            "version-failure" { @("--version") }
            default { @("--version", "--show-stats") }
        }
        if ((@($cacheCalls | ForEach-Object { $_.arguments }) -join " ") -cne ($expectedCacheCalls -join " ")) {
            throw "$Mode fixture: unexpected cache commands (shared daemon must remain untouched)"
        }
        if (@($cacheCalls | Where-Object { $_.wrapper }).Count -ne 0) {
            throw "$Mode fixture: stale wrapper remained during cache discovery"
        }
        foreach ($compiler in @("cargo", "rustc")) {
            $compilerCalls = @($calls | Where-Object { $_.command -eq $compiler })
            if ($compilerCalls.Count -ne 1 -or ($compilerCalls[0].arguments -join " ") -cne "--version") {
                throw "$Mode fixture: compiler version verification was skipped"
            }
        }
        if ($Mode -eq "healthy") {
            # Execute the actual YAML step: it must bypass both inherited env and
            # Cargo config wrappers, preserve -D warnings, and leave GITHUB_ENV alone.
            function cargo { & (Join-Path $fixtureRoot "cargo.ps1") @args }
            $exportsBeforeClippy = Get-Content -LiteralPath $env:GITHUB_ENV -Raw
            Push-Location $repoRoot
            try {
                & $clippyScript
                $env:RUSTC_WRAPPER = $fixtureSccache
                "clippy-failure" | Set-Content -LiteralPath (Join-Path $fixtureRoot "mode.txt") -Encoding utf8
                $clippyFailed = $false
                try { & $clippyScript } catch {
                    if ($_.Exception.Message -notlike "Cargo step failed: Windows Clippy (exit code 1)") { throw }
                    $clippyFailed = $true
                }
                if (-not $clippyFailed) { throw "Strict Clippy failure was suppressed." }
            } finally {
                Pop-Location
            }
            if ((Get-Content -LiteralPath $env:GITHUB_ENV -Raw) -cne $exportsBeforeClippy) {
                throw "Clippy bypass leaked into subsequent steps' exported build wrapper."
            }
            $clippyCalls = @(Get-Content -LiteralPath (Join-Path $fixtureRoot "calls.jsonl") |
                ForEach-Object { $_ | ConvertFrom-Json } |
                Where-Object { $_.command -eq "cargo" -and $_.arguments -contains "clippy" })
            if ($clippyCalls.Count -ne 2) { throw "Unexpected Clippy retry or missing invocation." }
            foreach ($call in $clippyCalls) {
                if ($call.wrapper -or ($call.arguments -join " ") -cne '--config build.rustc-wrapper="" clippy --workspace --all-targets -- -D warnings') {
                    throw "Clippy did not preserve strictness and bypass both wrapper sources."
                }
            }
            Write-Output "Windows Clippy step fixtures passed: success and strict failure"
        }
        Write-Output "CI setup fixture passed: $Mode"
    }
    finally {
        foreach ($name in $environmentNames) {
            [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], "Process")
        }
        if ($hadExitCode) {
            $global:LASTEXITCODE = $savedExitCode
        } else {
            Remove-Variable -Name LASTEXITCODE -Scope Global -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $fixtureRoot) {
            Remove-Item -LiteralPath $fixtureRoot -Recurse -Force
        }
    }
}

foreach ($mode in @("healthy", "mismatched", "transport-failure", "probe-exception", "version-failure", "absent")) {
    Test-AgentBusCiSetupFixture -Mode $mode
}
Write-Output "Rust build helper regression fixtures passed."
