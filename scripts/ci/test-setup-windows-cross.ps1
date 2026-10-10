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
# Exercise the production linker lookup with two actual PATH candidates. Linux
# images can expose the same compiler through both /usr/bin and /bin.
$lookupRoot = Join-Path ([IO.Path]::GetTempPath()) ('cross-path-fixture-' + [guid]::NewGuid().ToString('N'))
$priorPath = $env:PATH
try {
    $directories = @((Join-Path $lookupRoot 'first'), (Join-Path $lookupRoot 'second'))
    $nativeName = 'x86_64-w64-mingw32-gcc-posix' + $(if ($IsWindows) { '.exe' } else { '' })
    foreach ($directory in $directories) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
        $candidatePath = Join-Path $directory $nativeName
        [IO.File]::WriteAllText($candidatePath, "#!/bin/sh`nexit 0`n")
        if (-not $IsWindows) {
            [IO.File]::SetUnixFileMode($candidatePath, [IO.UnixFileMode]::UserRead -bor
                [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute)
        }
    }
    $env:PATH = $directories -join [IO.Path]::PathSeparator
    $candidates = @(Get-Command x86_64-w64-mingw32-gcc-posix -CommandType Application -ErrorAction Stop)
    Assert-SetupFixture ($candidates.Count -eq 2) 'duplicate PATH exposes two native compiler candidates'
    $lookup = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.AssignmentStatementAst] -and
        $node.Left.Extent.Text -ceq '$linker'
    }, $false)
    if (-not $lookup) { throw 'Production linker lookup missing' }
    . ([scriptblock]::Create($lookup.Extent.Text))
    Assert-SetupFixture ($linker -is [string] -and $linker -ceq $candidates[0].Source) 'production linker lookup selects the first native path'
} finally {
    $env:PATH = $priorPath
    foreach ($directory in $directories) {
        Remove-Item -LiteralPath (Join-Path $directory $nativeName) -ErrorAction Stop
        Remove-Item -LiteralPath $directory -ErrorAction Stop
    }
    Remove-Item -LiteralPath $lookupRoot -ErrorAction Stop
}
$image = 'sha256:' + ('b' * 64); $revision = 'c' * 40; $dockerfile = 'd' * 64
$container = [pscustomobject]@{ id = ('a' * 64); running = $true; image = $image }
$labels = [pscustomobject]@{ revision = $revision; dockerfile = $dockerfile }
$ns = [pscustomobject]@{ boot = '12345678-1234-1234-1234-123456789abc'; net = 'net:[123]'; pid = 'pid:[456]' }
Assert-CrossRunnerBinding $container ('a' * 12) $image $revision $dockerfile $labels $ns $ns
Assert-SetupFixture $true 'dynamic hostname resolves qualified full CID'
$rawImage = $container | ConvertTo-Json | ConvertFrom-Json
$rawImage.image = 'b' * 64
Assert-CrossRunnerBinding $rawImage ('a' * 12) $image $revision $dockerfile $labels $ns $ns
Assert-SetupFixture $true 'exact raw config digest resolves to the same qualified image'
foreach ($invalidImage in @(('e' * 64), ('B' * 64), ('b' * 63), ('b' * 65),
        (' ' + ('b' * 64)), (('b' * 64) + "`n"), ('sha256:' + ('B' * 64)),
        ($image + ' '), 'repo:latest')) {
    $bad = $container | ConvertTo-Json | ConvertFrom-Json; $bad.image = $invalidImage
    Reject-SetupFixture {
        Assert-CrossRunnerBinding $bad ('a' * 12) $image $revision $dockerfile $labels $ns $ns
    } 'wrong or noncanonical config digest refused'
}
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
$owner = [pscustomobject]@{ schema_version = 1; owner = 'agent-hub-windows-gnu-cache-v1'; uid = '1000'; version = '0.18.0'; image = $image; revision = $revision; dockerfile = $dockerfile; target = 'x86_64-pc-windows-gnu'; directory = (Join-Path $first 'data'); config = (Join-Path $first 'config'); config_sha256 = ('e' * 64); executable_sha256 = ('f' * 64); idle_timeout = '1800' }
Assert-CrossCacheMarker $owner $owner
Assert-SetupFixture $true 'dedicated owner/config admitted'
foreach ($field in @('uid', 'directory', 'config', 'executable_sha256', 'version', 'image', 'config_sha256', 'idle_timeout')) {
    $bad = $owner | ConvertTo-Json | ConvertFrom-Json
    $bad.$field = if ($field -eq 'directory') { Join-Path $second 'data' } else { 'different' }
    Reject-SetupFixture { Assert-CrossCacheMarker $bad $owner } "foreign cache marker $field refused"
}
$daemon = [pscustomobject]@{ pid = 123; start_ticks = '123456'; uid = '1000'; executable_sha256 = ('f' * 64); directory = $owner.directory; config = $owner.config; port = '4228'; idle_timeout = '1800' }
Assert-CrossCacheDaemon $daemon $owner
Assert-SetupFixture $true 'dedicated daemon identity admitted'
foreach ($field in @('pid', 'start_ticks', 'uid', 'directory', 'config', 'port', 'executable_sha256', 'idle_timeout')) {
    $bad = $daemon | ConvertTo-Json | ConvertFrom-Json
    $bad.$field = if ($field -eq 'pid') { 0 } elseif ($field -eq 'directory') { Join-Path $second 'data' } else { 'different' }
    Reject-SetupFixture { Assert-CrossCacheDaemon $bad $owner } "wrong daemon $field refused"
}
$ownerAssignment = $ast.Find({ param($node)
    $node -is [Management.Automation.Language.AssignmentStatementAst] -and
    $node.Left.Extent.Text -ceq '$owner'
}, $false)
$ownerTable = $ownerAssignment.Find({ param($node)
    $node -is [Management.Automation.Language.HashtableAst]
}, $false)
$timeoutEntries = @($ownerTable.KeyValuePairs | Where-Object { $_.Item1.SafeGetValue() -ceq 'idle_timeout' })
Assert-SetupFixture ($timeoutEntries.Count -eq 1) 'production owner records one explicit cache idle timeout'
$ownedTimeout = . ([scriptblock]::Create($timeoutEntries[0].Item2.Extent.Text))
Assert-SetupFixture ($ownedTimeout -ceq '1800' -and [int]$ownedTimeout -gt 1500) 'owned bounded idle timeout exceeds uncached stage budget'
foreach ($timeout in @('600', '0', '1500', 'different')) {
    $bad = $daemon | ConvertTo-Json | ConvertFrom-Json; $bad.idle_timeout = $timeout
    Reject-SetupFixture { Assert-CrossCacheDaemon $bad $owner } "wrong daemon idle timeout $timeout refused"
}
$missing = $daemon | ConvertTo-Json | ConvertFrom-Json
$missing.PSObject.Properties.Remove('idle_timeout')
Reject-SetupFixture { Assert-CrossCacheDaemon $missing $owner } 'default-only daemon without explicit idle timeout refused'
$missing = $owner | ConvertTo-Json | ConvertFrom-Json
$missing.PSObject.Properties.Remove('idle_timeout')
Reject-SetupFixture { Assert-CrossCacheMarker $missing $owner } 'old owner without idle timeout refused'
$environment = @('SCCACHE_DIR=/owned/data', 'SCCACHE_CONF=/owned/config', 'SCCACHE_SERVER_PORT=4228',
    'SCCACHE_IDLE_TIMEOUT=1800', 'AWS_SECRET_ACCESS_KEY=synthetic-unrelated', 'SCCACHE_UNKNOWN=synthetic-unrelated') -join [char]0
$selected = Convert-CrossCacheEnvironment $environment
Assert-SetupFixture ($selected.Count -eq 4 -and $selected['SCCACHE_IDLE_TIMEOUT'] -ceq '1800' -and
    $selected['SCCACHE_DIR'] -ceq '/owned/data' -and $selected['SCCACHE_CONF'] -ceq '/owned/config' -and
    $selected['SCCACHE_SERVER_PORT'] -ceq '4228') 'actual safe environment parser retains only four daemon identity fields'
Assert-SetupFixture (-not $selected.ContainsKey('AWS_SECRET_ACCESS_KEY') -and
    -not $selected.ContainsKey('SCCACHE_UNKNOWN')) 'safe daemon readback excludes unrelated credential and cache keys'
foreach ($path in @('C:/foreign/cache', 'relative/cache', "/owned`ncache")) {
    Reject-SetupFixture { Assert-CrossPlainPath $path } 'non-plain Linux cache path refused'
}
$compilerKeys = @('RUSTC', 'CARGO_BUILD_RUSTC', 'RUSTC_WORKSPACE_WRAPPER',
    'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WRAPPER')
$bindingKeys = @($compilerKeys) + @('SCCACHE_IDLE_TIMEOUT')
$previous = @{}
$previousGitHubEnv = [Environment]::GetEnvironmentVariable('GITHUB_ENV')
$exportFile = [IO.Path]::GetTempFileName()
try {
    foreach ($key in $bindingKeys) {
        $previous[$key] = [Environment]::GetEnvironmentVariable($key)
        Set-Item -LiteralPath "Env:$key" -Value 'synthetic-hostile-compiler-or-wrapper'
    }
    $rustc = '/qualified/toolchain/rustc'
    $bindings = @($ast.FindAll({ param($node)
        $node -is [Management.Automation.Language.AssignmentStatementAst] -and
        $node.Left.Extent.Text -cin @('$env:RUSTC', '$env:CARGO_BUILD_RUSTC',
            '$env:RUSTC_WORKSPACE_WRAPPER', '$env:CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER',
            '$env:CARGO_BUILD_RUSTC_WRAPPER', '$env:SCCACHE_IDLE_TIMEOUT')
    }, $false))
    Assert-SetupFixture ($bindings.Count -eq 6) 'all compiler and cache lifetime override bindings exist'
    foreach ($binding in $bindings) { . ([scriptblock]::Create($binding.Extent.Text)) }
    foreach ($key in @('RUSTC', 'CARGO_BUILD_RUSTC')) {
        Assert-SetupFixture ([Environment]::GetEnvironmentVariable($key) -ceq $rustc) "verified compiler replaces $key override"
    }
    foreach ($key in @('RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WRAPPER')) {
        Assert-SetupFixture ((Test-Path -LiteralPath "Env:$key") -and
            [Environment]::GetEnvironmentVariable($key) -ceq '') "explicit empty $key disables inherited config"
    }
    Assert-SetupFixture ([Environment]::GetEnvironmentVariable('SCCACHE_IDLE_TIMEOUT') -ceq '1800') 'owned idle timeout replaces hostile inherited setting'
    $idleBinding = @($bindings | Where-Object { $_.Left.Extent.Text -ceq '$env:SCCACHE_IDLE_TIMEOUT' })
    $scrub = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.ForEachStatementAst] -and
        $node.Extent.Text.StartsWith('foreach ($name in @([Environment]::GetEnvironmentVariables().Keys))', [StringComparison]::Ordinal)
    }, $false)
    Assert-SetupFixture ($scrub -and $idleBinding.Count -eq 1 -and
        $idleBinding[0].Extent.StartOffset -gt $scrub.Extent.EndOffset) 'owned idle timeout assignment follows inherited environment scrub'
    $export = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.IfStatementAst] -and
        $node.Extent.Text.StartsWith('if ($env:GITHUB_ENV)', [StringComparison]::Ordinal)
    }, $false)
    if (-not $export) { throw 'Production GITHUB_ENV export missing' }
    $env:GITHUB_ENV = $exportFile
    . ([scriptblock]::Create($export.Extent.Text))
    $lines = @(Get-Content -LiteralPath $exportFile)
    foreach ($key in $bindingKeys) {
        $expected = if ($key -cin @('RUSTC', 'CARGO_BUILD_RUSTC')) { "$key=$rustc" }
            elseif ($key -ceq 'SCCACHE_IDLE_TIMEOUT') { "$key=1800" } else { "$key=" }
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
