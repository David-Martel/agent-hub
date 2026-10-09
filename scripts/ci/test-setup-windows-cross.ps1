#Requires -Version 7.4
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# Import actual function bodies only; never execute the setup entry point.
$errors = $null; $tokens = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    (Join-Path $PSScriptRoot 'setup-windows-cross.ps1'), [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'Cross setup source does not parse' }
foreach ($function in $ast.FindAll({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst]
}, $false)) { . ([scriptblock]::Create($function.Extent.Text)) }
$script:passed = 0
function Assert-SetupFixture([bool]$Condition, [string]$Name) {
    if (-not $Condition) { throw $Name }
    $script:passed++; Write-Output "PASS: $Name"
}
function Reject-SetupFixture([scriptblock]$Action, [string]$Name) {
    $rejected = $false
    try { & $Action | Out-Null } catch { $rejected = $true }
    Assert-SetupFixture $rejected $Name
}
$image = 'sha256:' + ('b' * 64); $revision = 'c' * 40; $dockerfile = 'd' * 64
$container = [pscustomobject]@{ id = ('a' * 64); running = $true; image = $image }
$labels = [pscustomobject]@{ revision = $revision; dockerfile = $dockerfile }
$ns = [pscustomobject]@{ boot = '12345678-1234-1234-1234-123456789abc'; net = 'net:[123]'; pid = 'pid:[456]' }
Assert-CrossRunnerBinding $container ('a' * 12) $image $revision $dockerfile $labels $ns $ns
Assert-SetupFixture $true 'dynamic hostname resolves qualified full CID'
foreach ($field in @('running', 'image', 'id')) {
    $bad = $container | ConvertTo-Json | ConvertFrom-Json
    $bad.$field = switch ($field) { running { $false } image { 'sha256:' + ('e' * 64) } id { 'f' * 64 } }
    Reject-SetupFixture { Assert-CrossRunnerBinding $bad ('a' * 12) $image $revision $dockerfile $labels $ns $ns } "wrong container $field refused"
}
$bad = $container | ConvertTo-Json | ConvertFrom-Json; $bad.running = 'true'
Reject-SetupFixture { Assert-CrossRunnerBinding $bad ('a' * 12) $image $revision $dockerfile $labels $ns $ns } 'non-boolean running refused'
foreach ($field in @('boot', 'net', 'pid')) {
    $bad = $ns | ConvertTo-Json | ConvertFrom-Json; $bad.$field = 'different'
    Reject-SetupFixture { Assert-CrossRunnerBinding $container ('a' * 12) $image $revision $dockerfile $labels $ns $bad } "different selected-daemon $field refused"
}
foreach ($field in @('revision', 'dockerfile')) {
    $bad = $labels | ConvertTo-Json | ConvertFrom-Json; $bad.$field = 'different'
    Reject-SetupFixture { Assert-CrossRunnerBinding $container ('a' * 12) $image $revision $dockerfile $bad $ns $ns } "different image $field refused"
}
# Evaluate the production path assignment alone, with no directory/daemon I/O.
# Reverting to one cross-image cache namespace must fail this control.
$assignment = $ast.Find({ param($node)
    $node -is [Management.Automation.Language.AssignmentStatementAst] -and
    $node.Left.Extent.Text -ceq '$cacheRoot'
}, $false)
if (-not $assignment) { throw 'Production cache-root assignment missing' }
$cacheParent = Join-Path ([IO.Path]::GetTempPath()) 'synthetic-windows-gnu0.18'
$imageId = $image
. ([scriptblock]::Create($assignment.Extent.Text)); $first = $cacheRoot
$imageId = 'sha256:' + ('e' * 64)
. ([scriptblock]::Create($assignment.Extent.Text)); $second = $cacheRoot
Assert-SetupFixture ($first -cne $second -and (Split-Path $first -Leaf) -ceq ('b' * 64) -and
    (Split-Path $second -Leaf) -ceq ('e' * 64)) 'immutable images preserve distinct cache children'
$owner = [pscustomobject]@{ schema_version = 1; owner = 'agent-hub-windows-gnu-cache-v1'; uid = '1000'; version = '0.18.0'; image = $image; revision = $revision; dockerfile = $dockerfile; target = 'x86_64-pc-windows-gnu'; directory = (Join-Path $first 'data'); config = (Join-Path $first 'config'); config_sha256 = ('e' * 64); executable_sha256 = ('f' * 64) }
Assert-CrossCacheMarker $owner $owner
Assert-SetupFixture $true 'dedicated owner/config admitted'
foreach ($field in @('uid', 'directory', 'config', 'executable_sha256', 'version', 'image', 'config_sha256')) {
    $bad = $owner | ConvertTo-Json | ConvertFrom-Json
    $bad.$field = if ($field -eq 'directory') { Join-Path $second 'data' } else { 'different' }
    Reject-SetupFixture { Assert-CrossCacheMarker $bad $owner } "foreign cache marker $field refused"
}
$daemon = [pscustomobject]@{ pid = 123; start_ticks = '123456'; uid = '1000'; executable_sha256 = ('f' * 64); directory = $owner.directory; config = $owner.config; port = '4228' }
Assert-CrossCacheDaemon $daemon $owner
Assert-SetupFixture $true 'dedicated daemon identity admitted'
foreach ($field in @('pid', 'start_ticks', 'uid', 'directory', 'config', 'port', 'executable_sha256')) {
    $bad = $daemon | ConvertTo-Json | ConvertFrom-Json
    $bad.$field = if ($field -eq 'pid') { 0 } elseif ($field -eq 'directory') { Join-Path $second 'data' } else { 'different' }
    Reject-SetupFixture { Assert-CrossCacheDaemon $bad $owner } "wrong daemon $field refused"
}
foreach ($path in @('C:/foreign/cache', 'relative/cache', "/owned`ncache")) {
    Reject-SetupFixture { Assert-CrossPlainPath $path } 'non-plain Linux cache path refused'
}
$compilerKeys = @('RUSTC', 'CARGO_BUILD_RUSTC', 'RUSTC_WORKSPACE_WRAPPER',
    'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WRAPPER')
$previous = @{}
$previousGitHubEnv = [Environment]::GetEnvironmentVariable('GITHUB_ENV')
$exportFile = [IO.Path]::GetTempFileName()
try {
    foreach ($key in $compilerKeys) {
        $previous[$key] = [Environment]::GetEnvironmentVariable($key)
        Set-Item -LiteralPath "Env:$key" -Value 'synthetic-hostile-compiler-or-wrapper'
    }
    $rustc = '/qualified/toolchain/rustc'
    $bindings = @($ast.FindAll({ param($node)
        $node -is [Management.Automation.Language.AssignmentStatementAst] -and
        $node.Left.Extent.Text -cin @('$env:RUSTC', '$env:CARGO_BUILD_RUSTC',
            '$env:RUSTC_WORKSPACE_WRAPPER', '$env:CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER',
            '$env:CARGO_BUILD_RUSTC_WRAPPER')
    }, $false))
    Assert-SetupFixture ($bindings.Count -eq 5) 'all compiler override bindings exist'
    foreach ($binding in $bindings) { . ([scriptblock]::Create($binding.Extent.Text)) }
    foreach ($key in @('RUSTC', 'CARGO_BUILD_RUSTC')) {
        Assert-SetupFixture ([Environment]::GetEnvironmentVariable($key) -ceq $rustc) "verified compiler replaces $key override"
    }
    foreach ($key in @('RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WRAPPER')) {
        Assert-SetupFixture ((Test-Path -LiteralPath "Env:$key") -and
            [Environment]::GetEnvironmentVariable($key) -ceq '') "explicit empty $key disables inherited config"
    }
    $export = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.IfStatementAst] -and
        $node.Extent.Text.StartsWith('if ($env:GITHUB_ENV)', [StringComparison]::Ordinal)
    }, $false)
    if (-not $export) { throw 'Production GITHUB_ENV export missing' }
    $env:GITHUB_ENV = $exportFile
    . ([scriptblock]::Create($export.Extent.Text))
    $lines = @(Get-Content -LiteralPath $exportFile)
    foreach ($key in $compilerKeys) {
        $expected = if ($key -cin @('RUSTC', 'CARGO_BUILD_RUSTC')) { "$key=$rustc" } else { "$key=" }
        $matchesForKey = @($lines | Where-Object { $_.StartsWith("$key=", [StringComparison]::Ordinal) })
        Assert-SetupFixture ($matchesForKey.Count -eq 1 -and $matchesForKey[0] -ceq $expected) "exact compiler binding export for $key"
    }
} finally {
    foreach ($key in $previous.Keys) { Set-Item -LiteralPath "Env:$key" -Value $previous[$key] }
    Set-Item -LiteralPath Env:GITHUB_ENV -Value $previousGitHubEnv
    Remove-Item -LiteralPath $exportFile -ErrorAction Stop
}
Write-Output "SETUP_ADMISSION_SUMMARY passed=$script:passed failed=0"
# Reuse the existing finite-client fixture for both real source adapters.
& (Join-Path $PSScriptRoot 'test-cache-preflight.ps1') -Adapter Cache
& (Join-Path $PSScriptRoot 'test-cache-preflight.ps1') -Adapter Setup
