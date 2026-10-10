#Requires -Version 7.4
[CmdletBinding()]
param([ValidateSet('Cache', 'Setup')][string]$Adapter = 'Cache')
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'cache-preflight.ps1')
if ($Adapter -eq 'Setup') {
    $errors = $null; $tokens = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile(
        (Join-Path $PSScriptRoot 'setup-windows-cross.ps1'), [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw 'Cross setup source does not parse' }
    $function = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq 'Invoke-CrossSetupTool'
    }, $false)
    if (-not $function) { throw 'Production setup adapter not found' }
    . ([scriptblock]::Create($function.Extent.Text))
}
function Invoke-FixtureClient([string]$Script, [string[]]$Arguments, [int]$Timeout) {
    if ($Adapter -eq 'Cache') {
        Invoke-AgentBusCacheCommand $Script $Arguments $Timeout -Operation stats-after
    } else {
        Invoke-CrossSetupTool (Get-Process -Id $PID).Path (@('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $Script) + $Arguments) $Timeout
    }
}
function Complete-OwnedFixture([string]$Release, [string]$Exited) {
    [IO.File]::WriteAllText($Release, 'owned natural release')
    $settle = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path -LiteralPath $Exited) -and $settle.ElapsedMilliseconds -lt 3000) { Start-Sleep -Milliseconds 20 }
    if (-not (Test-Path -LiteralPath $Exited)) { throw 'Owned descendant did not settle naturally; fixture retained' }
    foreach ($retained in $script:AgentBusUnsettledClients) {
        $null = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@($retained.Stdout, $retained.Stderr)).Wait(2000)
        if (-not $retained.Process.HasExited -or -not $retained.Stdout.IsCompleted -or -not $retained.Stderr.IsCompleted) {
            throw 'Original client/drain settlement incomplete; exact handles retained'
        }
        $retained.Process.Dispose()
    }
    $script:AgentBusUnsettledClients.Clear()
}

# These shims exercise only the process adapter; they never invoke a live cache.
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('agent-bus-cache-pipes-' + [guid]::NewGuid())
$ready = Join-Path $fixtureRoot 'descendant.ready'
$release = Join-Path $fixtureRoot 'descendant.release'
$exited = Join-Path $fixtureRoot 'descendant.exited'
try {
    New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
    $slow = Join-Path $fixtureRoot 'pipe-holder.ps1'
    [IO.File]::WriteAllText($slow, @'
param([string]$Ready, [string]$Release, [string]$Exited, [string]$ReleaseAt)
$ErrorActionPreference = 'Stop'
[IO.File]::WriteAllText($Ready, 'owned descendant ready')
$limit = [DateTime]::UtcNow.AddSeconds(20)
while (-not (Test-Path -LiteralPath $Release)) {
    if ($ReleaseAt -and [DateTime]::UtcNow -ge [DateTime]::Parse($ReleaseAt).ToUniversalTime()) { break }
    if ([DateTime]::UtcNow -ge $limit) { break }
    Start-Sleep -Milliseconds 20
}
[IO.File]::WriteAllText($Exited, 'natural exit')
exit 0
'@)
    $parent = Join-Path $fixtureRoot 'parent.ps1'
    $source = @'
param([string]$ChildScript, [string]$Ready, [string]$Release, [string]$Exited, [string]$ExitAt, [string]$ReleaseAt)
$ErrorActionPreference = 'Stop'
$child = [Diagnostics.Process]::new()
$child.StartInfo.FileName = (Get-Process -Id $PID).Path
$child.StartInfo.UseShellExecute = $false
$child.StartInfo.CreateNoWindow = $true
foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $ChildScript,
        '-Ready', $Ready, '-Release', $Release, '-Exited', $Exited, '-ReleaseAt', $ReleaseAt)) {
    $child.StartInfo.ArgumentList.Add($argument)
}
if (-not $child.Start()) { throw 'Pipe fixture child failed to start' }
$limit = [DateTime]::UtcNow.AddSeconds(8)
while (-not (Test-Path -LiteralPath $Ready)) {
    if ($child.HasExited -or [DateTime]::UtcNow -ge $limit) { throw 'Descendant readiness failed' }
    Start-Sleep -Milliseconds 20
}
if ($ExitAt) { while ([DateTime]::UtcNow -lt [DateTime]::Parse($ExitAt).ToUniversalTime()) { Start-Sleep -Milliseconds 10 } }
[Console]::Out.WriteLine('owned parent exits successfully')
exit 0
'@
    [IO.File]::WriteAllText($parent, $source)
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $failure = $null
    try {
        $null = Invoke-FixtureClient $parent @('-ChildScript', $slow, '-Ready', $ready, '-Release', $release, '-Exited', $exited) 10
    } catch { $failure = $_.Exception.Message }
    $clock.Stop()
    if (-not (Test-Path -LiteralPath $ready -PathType Leaf)) { throw 'Owned descendant readiness not established' }
    if ($clock.Elapsed.TotalSeconds -gt 10 -or (Test-Path -LiteralPath $exited)) {
        throw 'Successful-parent pipe timeout escaped its bound or killed the descendant'
    }
    if ($null -eq $failure -or ($Adapter -eq 'Cache' -and ($failure -notlike 'Cache preflight client exited with open output pipes; daemon left untouched (operation=stats-after client_pid=* elapsed_ms=*)*' -or
        $failure -notmatch '<stdout pipe remains open>'))) {
        throw "Missing bounded successful-parent pipe diagnostics: $failure"
    }
    if ($script:AgentBusUnsettledClients.Count -ne 1) { throw 'Original process handle/drains were not retained' }
    $retained = $script:AgentBusUnsettledClients[0]
    if (-not $retained.Process.HasExited -or $retained.Process.ExitCode -ne 0 -or
        $retained.BornUtc -eq 'NOT_ESTABLISHED' -or $retained.Stdout.IsCompleted) {
        throw 'Original exited-client identity/open-pipe evidence was lost'
    }
    Complete-OwnedFixture $release $exited
    Write-Output "$Adapter pipe fixture passed: exited original handle retained, descendant untouched and settled naturally"

    $ready = Join-Path $fixtureRoot 'late.ready'
    $release = Join-Path $fixtureRoot 'late.release'
    $exited = Join-Path $fixtureRoot 'late.exited'
    $now = [DateTime]::UtcNow
    $arguments = @('-ChildScript', $slow, '-Ready', $ready, '-Release', $release, '-Exited', $exited,
        '-ExitAt', $now.AddMilliseconds(4200).ToString('o'), '-ReleaseAt', $now.AddMilliseconds(5300).ToString('o'))
    $clock = [Diagnostics.Stopwatch]::StartNew(); $failure = $null
    try { Invoke-FixtureClient $parent $arguments 5 | Out-Null } catch { $failure = $_.Exception.Message }
    if ($null -eq $failure -or $clock.ElapsedMilliseconds -lt 5000 -or $clock.ElapsedMilliseconds -gt 7200 -or
        -not (Test-Path -LiteralPath $ready) -or -not (Test-Path -LiteralPath $exited) -or
        $script:AgentBusUnsettledClients.Count -ne 0) { throw 'Fully exited/drained late-success control was not rejected at total deadline' }
    if ($Adapter -eq 'Cache' -and ($failure -notlike 'Cache preflight client timed out; daemon left untouched*' -or
        $failure -match '<stdout pipe remains open>')) { throw 'Fully drained late success failed for the wrong reason' }
    Write-Output "$Adapter final deadline fixture passed: fully settled late success rejected without killing descendants"
} finally {
    if (Test-Path -LiteralPath $ready) { Complete-OwnedFixture $release $exited }
    $resolvedRoot = [IO.Path]::GetFullPath($fixtureRoot)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedRoot.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path $resolvedRoot -Leaf) -notmatch '^agent-bus-cache-pipes-[a-f0-9-]{36}$') { throw 'Fixture cleanup path escaped temporary root' }
    # No PID lookup, descendant/tree kill, or unbounded WaitForExit. Incomplete
    # descendants/handles keep their artifacts for reconciliation.
    if ($script:AgentBusUnsettledClients.Count -eq 0 -and (Test-Path -LiteralPath $ready) -and
        -not @(Get-ChildItem -LiteralPath $fixtureRoot -Filter '*.ready' | Where-Object {
            -not (Test-Path -LiteralPath ($_.FullName -replace '\.ready$', '.exited'))
        }).Count) { Remove-Item -LiteralPath $resolvedRoot -Recurse }
}
