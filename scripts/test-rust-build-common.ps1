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

$originalDisableFlag = $script:AgentBusDisableSccacheForCargoSteps
$originalWrappers = @{}
$wrapperNames = @('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER',
    'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER')
foreach ($name in $wrapperNames) { $originalWrappers[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
try {
    foreach ($name in $wrapperNames) { [Environment]::SetEnvironmentVariable($name, 'fixture-sccache', 'Process') }
    Disable-AgentBusSccacheForCargoSteps
    foreach ($name in $wrapperNames) {
        if (Test-Path "Env:$name") { throw "Explicit disable did not clear wrapper: $name" }
    }
    if (-not $script:AgentBusDisableSccacheForCargoSteps) { throw 'Explicit disable did not enable Cargo config override' }
}
finally {
    $script:AgentBusDisableSccacheForCargoSteps = $originalDisableFlag
    foreach ($name in $wrapperNames) {
        if ($null -eq $originalWrappers[$name]) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
        else { Set-Item "Env:$name" $originalWrappers[$name] }
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
function Assert-AgentBusCrossClippyStep {
    param([Parameter(Mandatory)][string]$Workflow)
    $step = [regex]::Match($Workflow,
        '(?m)^      - name: Strict Windows Clippy without shared cache\r?\n        timeout-minutes: 25\r?\n        env:\r?\n          RUSTC_WRAPPER: "(?<wrapper>[^"\r\n]*)"\r?\n        run: (?<command>[^\r\n]+)\r?\n')
    if (-not $step.Success) { throw 'Expected strict GNU Windows Clippy step was not found.' }
    if ($step.Groups['wrapper'].Value -cne '') { throw 'GNU Windows Clippy must disable the cache wrapper.' }
    if ($step.Groups['command'].Value -cne 'cargo clippy --target "$WINDOWS_TARGET" --workspace --all-targets -- -D warnings') {
        throw 'GNU Windows Clippy target or strict warning flags differ.'
    }
}
Assert-AgentBusCrossClippyStep $workflowText
Write-Output 'Current GNU Windows Clippy declaration passed: target, wrapper and strictness'
foreach ($case in @(
    @{ name = 'wrong target'; text = $workflowText.Replace('--target "$WINDOWS_TARGET"', '--target x86_64-pc-windows-msvc') },
    @{ name = 'nonempty wrapper'; text = $workflowText.Replace('RUSTC_WRAPPER: ""', 'RUSTC_WRAPPER: "fixture-cache"') },
    @{ name = 'missing strict warnings'; text = $workflowText.Replace('-- -D warnings', '--') }
)) {
    $refused = $false
    try { Assert-AgentBusCrossClippyStep $case.text } catch {
        if ($_.Exception.Message -notlike 'GNU Windows Clippy*') { throw }
        $refused = $true
    }
    if (-not $refused) { throw "GNU Clippy declaration defect admitted: $($case.name)" }
    Write-Output "GNU Windows Clippy declaration refused: $($case.name)"
}

# Retain native helper regressions independently of the GNU job's Bash command.
# The real target-specific GNU command runs earlier in CI; these fixtures exercise
# explicit uncached helper behaviour with native disposable Cargo applications.
$clippyScript = [scriptblock]::Create(@'
. ./scripts/rust-build-common.ps1
Disable-AgentBusSccacheForCargoSteps
Invoke-AgentBusCargo -Label "Windows Clippy" -Command clippy -AdditionalArgs @("--workspace", "--all-targets", "--", "-D", "warnings")
'@)

# Resolve the repository's native compiler before any fixture changes USERPROFILE
# or RUSTUP_HOME. Windows rustup otherwise discovers the synthetic empty home.
$nativeFixtureCargo = Resolve-AgentBusNativeCargo
$nativeFixtureCompiler = Join-Path (Split-Path -Parent $nativeFixtureCargo.Path) $(if ($IsWindows) { 'rustc.exe' } else { 'rustc' })
if (-not (Test-Path -LiteralPath $nativeFixtureCompiler -PathType Leaf)) { throw 'Fixture native compiler unavailable beside pinned Cargo' }

function New-AgentBusNativeCargoFixture {
    param([Parameter(Mandatory)][string]$Directory, [Parameter(Mandatory)][string]$CompilerPath)
    $rustc = @(Microsoft.PowerShell.Core\Get-Command $CompilerPath -CommandType Application -ErrorAction Stop)[0].Source
    $hostLine = @(& $rustc -vV | Where-Object { $_ -like 'host: *' })
    if ($LASTEXITCODE -ne 0 -or $hostLine.Count -ne 1) { throw 'Fixture compiler host unavailable' }
    $hostTriple = $hostLine[0].Substring(6)
    $rustupHome = Join-Path $Directory 'private-rustup'
    $bin = Join-Path $rustupHome "toolchains/1.98.1-$hostTriple/bin"
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    $cargo = Join-Path $bin $(if ($IsWindows) { 'cargo.exe' } else { 'cargo' })
    $cache = Join-Path $Directory $(if ($IsWindows) { 'fixture-cache.exe' } else { 'fixture-cache' })
    $source = Join-Path $Directory 'fixture-cargo.rs'
    @'
use std::{env, fs::OpenOptions, io::Write, path::Path, process::{Command, exit}};
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if Path::new(&env::args().next().unwrap()).file_stem().unwrap() == "fixture-cache" {
        std::fs::write(env::var("AGENT_BUS_FIXTURE_TRAFFIC").unwrap(), b"actual owned cache child executed").unwrap();
        return;
    }
    let keys = ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"];
    let values: Vec<String> = keys.iter().map(|key| env::var(key).unwrap_or_default()).collect();
    let mut file = OpenOptions::new().create(true).append(true).open(env::var("AGENT_BUS_FIXTURE_CALLS").unwrap()).unwrap();
    writeln!(file, "{{\"command\":\"cargo\",\"arguments\":{:?},\"wrapper\":{:?},\"workspace_wrapper\":{:?},\"build_wrapper\":{:?},\"build_workspace_wrapper\":{:?},\"toolchain\":{:?},\"cargo_raw\":{:?}}}",
        args, values[0], values[1], values[2], values[3], env::var("RUSTUP_TOOLCHAIN").unwrap_or_default(), env::var("CARGO_RAW").unwrap_or_default()).unwrap();
    for value in values.iter().filter(|value| !value.is_empty()) {
        Command::new(value).arg("--fixture-cache-traffic").status().unwrap();
    }
    let mode = std::fs::read_to_string(env::var("AGENT_BUS_FIXTURE_MODE").unwrap()).unwrap();
    if mode.trim() == "clippy-failure" && args.iter().any(|arg| arg == "clippy") {
        println!("error: fixture strict Clippy warning"); exit(1);
    }
    println!("fixture native cargo 1.98.1");
}
'@ | Set-Content -LiteralPath $source -Encoding utf8
    & $rustc --edition=2021 $source -o $cargo
    if ($LASTEXITCODE -ne 0) { throw 'Small standalone native Cargo fixture compilation failed' }
    Copy-Item -LiteralPath $cargo -Destination $cache
    return [pscustomobject]@{ Cargo=$cargo; Cache=$cache; RustupHome=$rustupHome }
}

function Test-AgentBusExplicitUncachedFixture {
    $fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('agent-bus-uncached-' + [guid]::NewGuid().ToString('N'))
    $wrapperNames = @('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER',
        'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER')
    $environmentNames = $wrapperNames + @('RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'USERPROFILE',
        'CARGO_TARGET_DIR', 'CARGO_INCREMENTAL', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER',
        'RUSTFLAGS', 'CARGO_RAW', 'AGENT_BUS_FIXTURE_CALLS', 'AGENT_BUS_FIXTURE_MODE', 'AGENT_BUS_FIXTURE_TRAFFIC')
    $saved = @{}
    foreach ($name in $environmentNames) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
    $savedFlag = $script:AgentBusDisableSccacheForCargoSteps
    try {
        New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
        $fixture = New-AgentBusNativeCargoFixture -Directory $fixtureRoot -CompilerPath $nativeFixtureCompiler
        $env:RUSTUP_HOME = $fixture.RustupHome
        $env:RUSTUP_TOOLCHAIN = 'contrary-image-override'
        $env:USERPROFILE = $fixtureRoot
        $env:AGENT_BUS_FIXTURE_CALLS = Join-Path $fixtureRoot 'calls.jsonl'
        $env:AGENT_BUS_FIXTURE_MODE = Join-Path $fixtureRoot 'mode.txt'
        $env:AGENT_BUS_FIXTURE_TRAFFIC = Join-Path $fixtureRoot 'cache-traffic'
        'healthy' | Set-Content -LiteralPath $env:AGENT_BUS_FIXTURE_MODE
        foreach ($name in $wrapperNames) { [Environment]::SetEnvironmentVariable($name, $fixture.Cache, 'Process') }
        $script:AgentBusDisableSccacheForCargoSteps = $false
        $omitted = Use-AgentBusRustBuildEnv -RepoRoot $repoRoot -TargetDir (Join-Path $fixtureRoot 'omitted')
        if ($script:AgentBusDisableSccacheForCargoSteps) { throw 'Omitted cache preference disabled a custom wrapper' }
        foreach ($name in $wrapperNames) {
            if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $fixture.Cache) { throw 'Omitted preference removed a custom wrapper' }
        }
        Restore-AgentBusRustBuildEnv -State $omitted
        $state = Use-AgentBusRustBuildEnv -RepoRoot $repoRoot -TargetDir (Join-Path $fixtureRoot 'uncached') -PreferSccache:$false
        $initiallyDisabled = $script:AgentBusDisableSccacheForCargoSteps -and
            @($wrapperNames | Where-Object { [Environment]::GetEnvironmentVariable($_, 'Process') }).Count -eq 0
        foreach ($name in $wrapperNames) { [Environment]::SetEnvironmentVariable($name, $fixture.Cache, 'Process') }
        $env:CARGO_RAW = 'original-cargo-raw'
        function Invoke-CargoToolsReinitializingFixture {
            'alias executed' | Set-Content -LiteralPath (Join-Path $fixtureRoot 'alias-traffic')
            $env:RUSTC_WRAPPER = $fixture.Cache
            & $fixture.Cargo @args
        }
        Set-Alias -Name cargo -Value Invoke-CargoToolsReinitializingFixture -Scope Local
        $result = Invoke-AgentBusRawCargo -Command clippy -AdditionalArgs @('--workspace', '--all-targets', '--', '-D', 'warnings') -DisableSccache
        Write-Output "Uncached control: initially_disabled=$initiallyDisabled alias_traffic=$(Test-Path -LiteralPath (Join-Path $fixtureRoot 'alias-traffic')) cache_child_traffic=$(Test-Path -LiteralPath $env:AGENT_BUS_FIXTURE_TRAFFIC)"
        if (-not $initiallyDisabled -or $result.ExitCode -ne 0 -or
            (Test-Path -LiteralPath (Join-Path $fixtureRoot 'alias-traffic')) -or
            (Test-Path -LiteralPath $env:AGENT_BUS_FIXTURE_TRAFFIC)) {
            throw 'Explicit uncached build leaked its inherited wrappers or executed the reinitializing alias/cache child'
        }
        $call = Get-Content -LiteralPath $env:AGENT_BUS_FIXTURE_CALLS -Raw | ConvertFrom-Json
        if ($call.wrapper -or $call.workspace_wrapper -or $call.build_wrapper -or $call.build_workspace_wrapper -or
            $call.toolchain -cne '1.98.1' -or ($call.arguments -join ' ') -cne
            '--config build.rustc-wrapper="" --config build.rustc-workspace-wrapper="" clippy --workspace --all-targets -- -D warnings') {
            throw 'Native uncached child did not receive pinned compiler and both empty wrapper configs'
        }
        foreach ($name in $wrapperNames) {
            if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $fixture.Cache) { throw 'Successful native call did not restore wrapper environment' }
        }
        if ($env:RUSTUP_TOOLCHAIN -cne 'contrary-image-override') { throw 'Native Cargo call leaked its temporary toolchain override' }
        'clippy-failure' | Set-Content -LiteralPath $env:AGENT_BUS_FIXTURE_MODE
        $failed = $false
        try { Invoke-AgentBusCargo -Label 'explicit uncached fixture' -Command clippy -AdditionalArgs @('--', '-D', 'warnings') }
        catch {
            if ($_.Exception.Message -cne 'Cargo step failed: explicit uncached fixture (exit code 1)') { throw }
            $failed = $true
        }
        if (-not $failed -or $env:CARGO_RAW -cne 'original-cargo-raw') { throw 'Uncached failure was suppressed or leaked CargoTools state' }
        foreach ($name in $wrapperNames) {
            if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $fixture.Cache) { throw 'Failed native call did not restore wrapper environment' }
        }
        Restore-AgentBusRustBuildEnv -State $state
        if ($script:AgentBusDisableSccacheForCargoSteps) { throw 'Build scope did not restore the original cache flag' }
        foreach ($name in $wrapperNames) {
            if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $fixture.Cache) { throw 'Build scope did not restore original wrapper values' }
        }
        if ((Test-Path -LiteralPath (Join-Path $fixtureRoot 'alias-traffic')) -or
            (Test-Path -LiteralPath $env:AGENT_BUS_FIXTURE_TRAFFIC)) { throw 'Failure path executed alias or cache child' }
        foreach ($name in $wrapperNames) { [Environment]::SetEnvironmentVariable($name, '', 'Process') }
        $emptyState = Use-AgentBusRustBuildEnv -RepoRoot $repoRoot -TargetDir (Join-Path $fixtureRoot 'empty-values') -PreferSccache:$false
        $mixed = @{}
        foreach ($name in $wrapperNames) {
            $mixed[$name] = $(if ($name -like '*WORKSPACE*') { $null } else { '' })
            if ($null -eq $mixed[$name]) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
            else { Set-Item "Env:$name" $mixed[$name] }
        }
        $emptyResult = Invoke-AgentBusRawCargo -Command clippy -AdditionalArgs @('--', '-D', 'warnings') -DisableSccache
        if ($emptyResult.ExitCode -ne 1) { throw 'Empty/absent restoration control did not execute the failing native child' }
        foreach ($name in $wrapperNames) {
            $restored = [Environment]::GetEnvironmentVariable($name, 'Process')
            if (($null -eq $restored) -ne ($null -eq $mixed[$name]) -or $restored -cne $mixed[$name]) {
                throw "Failed native invocation did not distinguish empty/absent wrapper: $name"
            }
        }
        Restore-AgentBusRustBuildEnv -State $emptyState
        foreach ($name in $wrapperNames) {
            $restored = [Environment]::GetEnvironmentVariable($name, 'Process')
            if ($null -eq $restored -or $restored -cne '') { throw 'Build scope did not distinguish empty wrapper values from absent variables' }
        }
        Write-Output 'Explicit uncached native Cargo fixtures passed: custom-wrapper preservation, alias/cache isolation, pinned toolchain, success/failure, scope restoration and empty-value preservation'
    }
    finally {
        foreach ($name in $environmentNames) {
            if ($null -eq $saved[$name]) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
            else { Set-Item "Env:$name" $saved[$name] }
        }
        $script:AgentBusDisableSccacheForCargoSteps = $savedFlag
        $resolved = [IO.Path]::GetFullPath($fixtureRoot)
        if (-not $resolved.StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()), [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture cleanup escaped temporary storage' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}


function Test-AgentBusCiSetupFixture {
    param([Parameter(Mandatory = $true)][string]$Mode)

    $fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("agent-bus-ci-fixture-" + [guid]::NewGuid())
    $environmentNames = @(
        "AGENT_HUB_CI_CACHE_ROOT", "LOCALAPPDATA", "USERPROFILE", "PATH",
        "GITHUB_ENV", "GITHUB_PATH", "GITHUB_JOB", "RUNNER_ARCH",
        "CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "SCCACHE_DIR", "SCCACHE_SERVER_PORT", "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "AGENT_BUS_FIXTURE_CALLS", "AGENT_BUS_FIXTURE_MODE", "AGENT_BUS_FIXTURE_TRAFFIC"
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
        if ($mode -eq "version-failure") { $global:LASTEXITCODE = 2; "version unavailable" }
        else { "sccache 0.18.0" }
    }
    "--show-stats" {
        if ($mode -eq "transport-failure") { throw "Failed to read response header (os error10060)" }
        if ($mode -eq "probe-exception") { throw "fixture health probe exception" }
        $counter = Join-Path $PSScriptRoot "compile-count.txt"
        $count = if (Test-Path -LiteralPath $counter) { [int](Get-Content -LiteralPath $counter) } else { 0 }
        $executed = if ($mode -eq "synthetic") { 0 } else { $count }
        $hits = if ($mode -eq "no-hit") { 0 } else { [Math]::Max(0, $count - 1) }
        $counts = if ($hits) { @{ Rust = $hits } } else { @{} }
        @{version = $(if ($mode -eq "mismatched") { "0.16.0" } else { "0.18.0" }); stats = @{
            requests_executed = $executed; cache_hits = @{counts = $counts};
            requests_not_cacheable = $(if ($mode -eq "non-cacheable") { $count } else { 0 });
            requests_unsupported_compiler = 0; cache_timeouts = 0; compile_fails = 0;
            cache_read_errors = $(if ($mode -eq "read-error") { $count } else { 0 }); cache_write_errors = 0;
            non_cacheable_compilations = 0; cache_errors = @{counts = $(if ($mode -eq "cache-error") { @{Rust = $count} } else { @{} })}
        }} | ConvertTo-Json -Depth 5 -Compress
    }
    default {
        if ($args[0] -notlike "*rustc.ps1" -or $args -notcontains "--emit=link") {
            throw "Unexpected cache invocation: $($args -join ' ')"
        }
        if ($mode -eq "compile-failure") { throw "fixture compiler failure" }
        $counter = Join-Path $PSScriptRoot "compile-count.txt"
        $count = if (Test-Path -LiteralPath $counter) { [int](Get-Content -LiteralPath $counter) } else { 0 }
        ($count + 1) | Set-Content -LiteralPath $counter
        $outputIndex = [Array]::IndexOf($args, "--out-dir")
        $artifact = Join-Path $args[$outputIndex + 1] "libagent_hub_cache_preflight.rlib"
        $(if ($mode -eq "artifact-drift") { "fixture artifact$count" } else { "fixture artifact" }) | Set-Content -LiteralPath $artifact
    }
}
exit $global:LASTEXITCODE
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
        $setupFailure = $null
        $cacheDiagnostics = @()
        try {
            $cacheDiagnostics = @(& $ciSetupPath 6>&1)
            $cacheDiagnostics | ForEach-Object { Write-Host $_ }
        } catch { $setupFailure = $_.Exception.Message }
        if ($Mode -ne "healthy") {
            if (-not $setupFailure) { throw "$Mode fixture: unhealthy cache silently admitted" }
            if ($env:RUSTC_WRAPPER) { throw "$Mode fixture: exported unhealthy wrapper" }
            $wrapperEntries = @(Get-Content -LiteralPath $env:GITHUB_ENV | Where-Object { $_ -like "RUSTC_WRAPPER=*" })
            if ($wrapperEntries.Count -ne 1) { throw "$Mode fixture: failed preflight exported GITHUB_ENV" }
            $compilerCalls = @(Get-Content -LiteralPath (Join-Path $fixtureRoot "calls.jsonl") |
                ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.command -in @("cargo", "rustc") })
            if ($compilerCalls.Count) { throw "$Mode fixture: compiler work continued after failed admission" }
            Write-Output "CI setup refusal passed: $Mode ($setupFailure)"
            return
        }
        if ($setupFailure) { throw "Healthy cache admission failed: $setupFailure" }
        $diagnostics = ($cacheDiagnostics | ForEach-Object { $_.ToString() }) -join "`n"
        foreach ($operation in @('stats-before', 'compile-first', 'compile-second', 'stats-after')) {
            if ($diagnostics -notmatch "operation=$operation state=started client_pid=[0-9]+ timeout_seconds=30" -or
                $diagnostics -notmatch "operation=$operation state=completed client_pid=[0-9]+ elapsed_ms=[0-9]+ exit_code=0") {
                throw "Missing bounded cache operation diagnostics: $operation"
            }
        }

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
        if ($cacheCalls.Count -ne 5 -or ($cacheCalls[0].arguments -join " ") -cne "--version" -or
            ($cacheCalls[1].arguments -join " ") -cne "--show-stats --stats-format=json" -or
            ($cacheCalls[4].arguments -join " ") -cne "--show-stats --stats-format=json") {
            throw "Expected version, baseline, two serial actual compilers and final real stats"
        }
        foreach ($call in @($cacheCalls[2], $cacheCalls[3])) {
            if ($call.arguments -notcontains "--emit=link" -or $call.arguments -notcontains "-Dwarnings") {
                throw "Actual cacheable strict compiler preflight missing"
            }
        }
        $proofPath = Join-Path $expectedTarget "cache-preflight/proof.json"
        $proof = Get-Content -LiteralPath $proofPath -Raw | ConvertFrom-Json
        if ($proof.after.stats.requests_executed -ne 2 -or $proof.after.stats.cache_hits.counts.Rust -ne 1 -or
            $proof.wrapper_fallback -ne $false) { throw "Real cache receipt missing" }
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
            # Execute the native helper: it must bypass both inherited env and
            # Cargo config wrappers, preserve -D warnings, and leave GITHUB_ENV alone.
            $nativeFixture = New-AgentBusNativeCargoFixture -Directory $fixtureRoot -CompilerPath $nativeFixtureCompiler
            $env:RUSTUP_HOME = $nativeFixture.RustupHome
            $env:RUSTUP_TOOLCHAIN = 'contrary-image-override'
            $env:AGENT_BUS_FIXTURE_CALLS = Join-Path $fixtureRoot 'calls.jsonl'
            $env:AGENT_BUS_FIXTURE_MODE = Join-Path $fixtureRoot 'mode.txt'
            $env:AGENT_BUS_FIXTURE_TRAFFIC = Join-Path $fixtureRoot 'native-cache-traffic'
            function Invoke-CargoToolsFixture {
                throw 'Strict Clippy executed a CargoTools-style alias instead of native Cargo'
            }
            Set-Alias -Name cargo -Value Invoke-CargoToolsFixture -Scope Local
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
                if ($call.wrapper -or ($call.arguments -join " ") -cne '--config build.rustc-wrapper="" --config build.rustc-workspace-wrapper="" clippy --workspace --all-targets -- -D warnings') {
                    throw "Clippy did not preserve strictness and bypass both wrapper sources."
                }
            }
            Write-Output "Native Windows Clippy helper fixtures passed: success and strict failure"
        }
        Write-Output "CI setup fixture passed: $Mode"
    }
    finally {
        foreach ($name in $environmentNames) {
            if ($null -eq $savedEnvironment[$name]) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
            else { Set-Item "Env:$name" $savedEnvironment[$name] }
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

Test-AgentBusExplicitUncachedFixture

foreach ($mode in @("healthy", "mismatched", "transport-failure", "probe-exception", "version-failure", "absent", "synthetic", "no-hit", "non-cacheable", "compile-failure", "read-error", "cache-error", "artifact-drift")) {
    Test-AgentBusCiSetupFixture -Mode $mode
}
. (Join-Path $PSScriptRoot "ci/cache-preflight.ps1")
$controlRoot = Join-Path ([IO.Path]::GetTempPath()) ("agent-bus-cache-controls-" + [guid]::NewGuid())
$foreign = $null
try {
    New-Item -ItemType Directory -Path $controlRoot | Out-Null
    $policy = Get-Content -LiteralPath (Join-Path $PSScriptRoot "ci/windows-cache-policy.json") -Raw | ConvertFrom-Json
    $ownedDirectory = Join-Path $controlRoot "cache-preflight"
    New-Item -ItemType Directory -Path $ownedDirectory | Out-Null
    $sentinel = Join-Path $ownedDirectory "foreign.txt"
    [IO.File]::WriteAllText($sentinel, "preserve foreign bytes")
    try {
        $null = Invoke-AgentBusCachePreflight -SccachePath "must-not-execute" -RustcPath "must-not-execute" -TargetDirectory $controlRoot -Policy $policy
        throw "Foreign directory admitted"
    } catch { if ($_.Exception.Message -cne "Refuse foreign cache preflight directory") { throw } }
    if ([IO.File]::ReadAllText($sentinel) -cne "preserve foreign bytes") { throw "Foreign file changed" }
    $policy.server_port = 4810
    try {
        $null = Invoke-AgentBusCachePreflight -SccachePath "must-not-execute" -RustcPath "must-not-execute" -TargetDirectory $controlRoot -Policy $policy
        throw "Shared cache port admitted"
    } catch { if ($_.Exception.Message -cne "Invalid dedicated Windows cache policy") { throw } }
    $slowScript = Join-Path $controlRoot "slow.ps1"
    [IO.File]::WriteAllText($slowScript, '[Console]::Out.WriteLine("fixture stdout before timeout"); [Console]::Error.WriteLine("fixture stderr before timeout"); Start-Sleep -Seconds 30')
    $foreign = [Diagnostics.Process]::new()
    $foreign.StartInfo.FileName = (Get-Process -Id $PID).Path
    $foreign.StartInfo.UseShellExecute = $false
    foreach ($argument in @('-NoLogo', '-NoProfile', '-File', $slowScript)) { $foreign.StartInfo.ArgumentList.Add($argument) }
    if (-not $foreign.Start()) { throw "Foreign control process failed to start" }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    try {
        # A cold pwsh shim takes several seconds to initialize on Windows.
        # Give the fixture time to emit its witness output before the timeout;
        # the production policy remains 30 seconds.
        $null = Invoke-AgentBusCacheCommand -Executable $slowScript -Arguments @() -TimeoutSeconds 5 -Operation compile-first
        throw "Client timeout not enforced"
    } catch {
        if ($_.Exception.Message -notlike 'Cache preflight client timed out; daemon left untouched (operation=compile-first client_pid=* elapsed_ms=*)*' -or
            $_.Exception.Message -notmatch 'stdout: fixture stdout before timeout' -or
            $_.Exception.Message -notmatch 'stderr: fixture stderr before timeout') { throw }
    }
    $clock.Stop()
    if ($clock.Elapsed.TotalSeconds -gt 9 -or $foreign.HasExited) { throw "Timeout escaped bound or terminated foreign process" }
    Write-Output "Cache controls passed: foreign path and shared port refused; bounded owned-client timeout preserved foreign process"
} finally {
    if ($foreign) {
        if (-not $foreign.HasExited) { $foreign.Kill(); $foreign.WaitForExit() }
        $foreign.Dispose()
    }
    Remove-Item -LiteralPath $controlRoot -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Output "Rust build helper regression fixtures passed."
& (Join-Path $PSScriptRoot 'ci/test-cache-preflight.ps1')

# Expected native failures above must not determine the successful test entrypoint exit.
# Uncaught assertion failures terminate before this explicit success result.
exit 0
