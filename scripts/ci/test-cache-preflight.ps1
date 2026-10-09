#Requires -Version 7.4
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'cache-preflight.ps1')

# These shims exercise only the process adapter; they never invoke a live cache.
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('agent-bus-cache-pipes-' + [guid]::NewGuid())
$descendant = $null
try {
    New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
    $slow = Join-Path $fixtureRoot 'pipe-holder.ps1'
    [IO.File]::WriteAllText($slow, '[Console]::Out.WriteLine("owned descendant retains output"); Start-Sleep -Seconds 30')
    $pidFile = Join-Path $fixtureRoot 'descendant.pid'
    $parent = Join-Path $fixtureRoot 'parent.ps1'
    $source = @'
param([string]$ChildScript, [string]$PidFile)
$child = [Diagnostics.Process]::new()
$child.StartInfo.FileName = (Get-Process -Id $PID).Path
$child.StartInfo.UseShellExecute = $false
$child.StartInfo.CreateNoWindow = $true
foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $ChildScript)) {
    $child.StartInfo.ArgumentList.Add($argument)
}
if (-not $child.Start()) { throw 'Pipe fixture child failed to start' }
[IO.File]::WriteAllText($PidFile, [string]$child.Id)
[Console]::Out.WriteLine('owned parent exits successfully')
'@
    [IO.File]::WriteAllText($parent, $source)
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $failure = $null
    try {
        $null = Invoke-AgentBusCacheCommand -Executable $parent -Arguments @($slow, $pidFile) -TimeoutSeconds 10 -Operation stats-after
    } catch { $failure = $_.Exception.Message }
    $clock.Stop()
    if (-not (Test-Path -LiteralPath $pidFile -PathType Leaf)) { throw 'Fixture did not record its owned descendant' }
    $descendant = Get-Process -Id ([int][IO.File]::ReadAllText($pidFile)) -ErrorAction Stop
    if ($clock.Elapsed.TotalSeconds -gt 10 -or $descendant.HasExited) {
        throw 'Successful-parent pipe timeout escaped its bound or killed the descendant'
    }
    if ($failure -notlike 'Cache preflight client exited with open output pipes; daemon left untouched (operation=stats-after client_pid=* elapsed_ms=*)*' -or
        $failure -notmatch '<stdout pipe remains open>') {
        throw "Missing bounded successful-parent pipe diagnostics: $failure"
    }
    Write-Output 'Cache pipe fixture passed: successful client exit fails visibly on retained stdout, preserving the descendant'
} finally {
    if ($descendant) {
        if (-not $descendant.HasExited) { $descendant.Kill(); $descendant.WaitForExit() }
        $descendant.Dispose()
    } elseif (Test-Path -LiteralPath $pidFile -PathType Leaf) {
        # The fixture itself owns this exact child, even when an earlier assertion fails.
        $child = Get-Process -Id ([int][IO.File]::ReadAllText($pidFile)) -ErrorAction SilentlyContinue
        if ($child) { $child.Kill(); $child.WaitForExit(); $child.Dispose() }
    }
    $resolvedRoot = [IO.Path]::GetFullPath($fixtureRoot)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (-not $resolvedRoot.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture cleanup path escaped temporary root' }
    Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
}
