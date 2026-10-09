#Requires -Version 7.4
Set-StrictMode -Version Latest

# Keep the original handle and drain tasks when settlement is incomplete. A
# caller can reconcile only this exact client later; never substitute a PID.
if (-not (Get-Variable AgentBusUnsettledClients -Scope Script -ErrorAction SilentlyContinue)) {
    $script:AgentBusUnsettledClients = [Collections.Generic.List[object]]::new()
}

function Invoke-AgentBusCacheCommand {
    param(
        [string]$Executable,
        [string[]]$Arguments,
        [int]$TimeoutSeconds,
        [Parameter(Mandatory)]
        [ValidateSet('stats-before', 'compile-first', 'compile-second', 'stats-after')]
        [string]$Operation
    )

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    # Script commands are legitimate PowerShell CLI shims and let fixtures exercise
    # the same bounded process adapter without touching installed compilers/caches.
    if ([IO.Path]::GetExtension($Executable) -eq '.ps1') {
        $process.StartInfo.FileName = (Get-Process -Id $PID).Path
        foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $Executable)) {
            $process.StartInfo.ArgumentList.Add($argument)
        }
    } else {
        $process.StartInfo.FileName = $Executable
    }
    foreach ($argument in $Arguments) { $process.StartInfo.ArgumentList.Add($argument) }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $started = $false
    $stdout = $stderr = $null
    $clientBornUtc = 'NOT_ESTABLISHED'
    try {
        if (-not $process.Start()) { throw 'Could not start cache preflight client' }
        $started = $true
        try { $clientBornUtc = $process.StartTime.ToUniversalTime().ToString('o') } catch {
            if (-not $process.HasExited) { throw }
        }
        Write-Host "Cache preflight operation=$Operation state=started client_pid=$($process.Id) timeout_seconds=$TimeoutSeconds"
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $clientPid = $process.Id
        $remainingMilliseconds = [int][Math]::Max(0, $TimeoutSeconds * 1000 - $clock.ElapsedMilliseconds)
        $timedOut = -not $process.WaitForExit($remainingMilliseconds)
        $timedOut = $timedOut -or $clock.ElapsedMilliseconds -ge $TimeoutSeconds * 1000
        if ($timedOut -and -not $process.HasExited) {
            # The original Process handle identifies our exact client; never kill a tree.
            $process.Kill($false)
        }
        # Process exit and both pipes share one bounded settlement allowance.
        $settlement = [Diagnostics.Stopwatch]::StartNew()
        if (-not $process.HasExited) { $null = $process.WaitForExit(2000) }
        $captureTasks = [System.Threading.Tasks.Task[]]@($stdout, $stderr)
        $allOutput = [System.Threading.Tasks.Task]::WhenAll($captureTasks)
        $remainingSettlement = [int][Math]::Max(0, 2000 - $settlement.ElapsedMilliseconds)
        $null = $allOutput.Wait($remainingSettlement)
        $stdoutComplete = $stdout.IsCompletedSuccessfully
        $stderrComplete = $stderr.IsCompletedSuccessfully
        $timedOut = $timedOut -or $clock.ElapsedMilliseconds -ge $TimeoutSeconds * 1000
        if ($timedOut) {
            $clock.Stop()
            $output = if ($stdoutComplete) { $stdout.GetAwaiter().GetResult() } else { '<stdout pipe remains open>' }
            $errorOutput = if ($stderrComplete) { $stderr.GetAwaiter().GetResult() } else { '<stderr pipe remains open>' }
            Write-Host "Cache preflight operation=$Operation state=timeout client_pid=$clientPid born_utc=$clientBornUtc elapsed_ms=$($clock.ElapsedMilliseconds)"
            throw "Cache preflight client timed out; daemon left untouched (operation=$Operation client_pid=$clientPid born_utc=$clientBornUtc elapsed_ms=$($clock.ElapsedMilliseconds))`nstdout: $output`nstderr: $errorOutput"
        }
        $result = [pscustomobject]@{
            ExitCode = $process.ExitCode
            Output = if ($stdoutComplete) { $stdout.GetAwaiter().GetResult() } else { '<stdout pipe remains open>' }
            Error = if ($stderrComplete) { $stderr.GetAwaiter().GetResult() } else { '<stderr pipe remains open>' }
        }
        $clock.Stop()
        if (-not $stdoutComplete -or -not $stderrComplete) {
            Write-Host "Cache preflight operation=$Operation state=pipe-timeout client_pid=$($process.Id) elapsed_ms=$($clock.ElapsedMilliseconds) exit_code=$($result.ExitCode)"
            throw "Cache preflight client exited with open output pipes; daemon left untouched (operation=$Operation client_pid=$($process.Id) elapsed_ms=$($clock.ElapsedMilliseconds))`nstdout: $($result.Output)`nstderr: $($result.Error)"
        }
        Write-Host "Cache preflight operation=$Operation state=completed client_pid=$($process.Id) elapsed_ms=$($clock.ElapsedMilliseconds) exit_code=$($result.ExitCode)"
        if ($result.ExitCode -ne 0) {
            throw "Cache preflight client failed ($($result.ExitCode), operation=$Operation client_pid=$($process.Id) elapsed_ms=$($clock.ElapsedMilliseconds))`nstdout: $($result.Output)`nstderr: $($result.Error)"
        }
        return $result
    } finally {
        if ($started -and (-not $process.HasExited -or
                ($stdout -and -not $stdout.IsCompleted) -or ($stderr -and -not $stderr.IsCompleted))) {
            $script:AgentBusUnsettledClients.Add([pscustomobject]@{
                Process = $process; ClientPid = $process.Id; BornUtc = $clientBornUtc
                Executable = $process.StartInfo.FileName; Stdout = $stdout; Stderr = $stderr
                ElapsedMilliseconds = $clock.ElapsedMilliseconds
            })
        } else { $process.Dispose() }
    }
}

function Invoke-AgentBusCachePreflight {
    param(
        [string]$SccachePath, [string]$RustcPath, [string]$TargetDirectory, $Policy,
        [ValidateSet('x86_64-pc-windows-gnu')][string]$TargetTriple
    )

    if ($Policy.schema_version -ne 1 -or $Policy.version -cne '0.18.0' -or
        $Policy.server_port -ne 4228 -or $Policy.command_timeout_seconds -lt 1 -or
        $Policy.command_timeout_seconds -gt 60 -or $Policy.preflight_directory -cne 'cache-preflight') {
        throw 'Invalid dedicated Windows cache policy'
    }
    $preflightLeaf = if ($TargetTriple) { 'cache-preflight-' + $TargetTriple } else { $Policy.preflight_directory }
    $directory = Join-Path $TargetDirectory $preflightLeaf
    $marker = Join-Path $directory 'owner.txt'
    $owner = if ($TargetTriple) { 'agent-hub-ci-cache-preflight-v1:' + $TargetTriple } else { 'agent-hub-ci-cache-preflight-v1' }
    if (Test-Path -LiteralPath $directory) {
        if (-not (Test-Path -LiteralPath $marker -PathType Leaf) -or
            [IO.File]::ReadAllText($marker) -cne $owner) { throw 'Refuse foreign cache preflight directory' }
    } else {
        New-Item -ItemType Directory -Path $directory | Out-Null
        [IO.File]::WriteAllText($marker, $owner, [Text.UTF8Encoding]::new($false))
    }
    $source = Join-Path $directory 'preflight.rs'
    $sourceText = 'pub fn agent_hub_cache_preflight() -> u64 { 42 }'
    if (Test-Path -LiteralPath $source) {
        if ([IO.File]::ReadAllText($source) -cne $sourceText) { throw 'Refuse foreign cache preflight source' }
    } else {
        [IO.File]::WriteAllText($source, $sourceText, [Text.UTF8Encoding]::new($false))
    }
    $timeout = [int]$Policy.command_timeout_seconds
    $before = (Invoke-AgentBusCacheCommand -Executable $SccachePath `
        -Arguments @('--show-stats', '--stats-format=json') -TimeoutSeconds $timeout -Operation stats-before).Output | ConvertFrom-Json
    # --show-stats can return successful synthetic zero stats with NO daemon.
    # A serial actual compiler request starts an absent dedicated daemon before
    # Cargo fans out. Existing daemons are never restarted or reset here.
    $arguments = @($RustcPath, '--crate-name', 'agent_hub_cache_preflight', '--edition=2024',
        $source, '--crate-type=rlib', '--emit=link', '--out-dir', $directory, '-Dwarnings')
    if ($TargetTriple) { $arguments += @('--target', $TargetTriple) }
    $null = Invoke-AgentBusCacheCommand -Executable $SccachePath -Arguments $arguments -TimeoutSeconds $timeout -Operation compile-first
    $artifact = Join-Path $directory 'libagent_hub_cache_preflight.rlib'
    if (-not (Test-Path -LiteralPath $artifact -PathType Leaf) -or
        (Get-Item -LiteralPath $artifact).Length -eq 0) { throw 'Cache preflight produced no artifact' }
    $firstHash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash
    $null = Invoke-AgentBusCacheCommand -Executable $SccachePath -Arguments $arguments -TimeoutSeconds $timeout -Operation compile-second
    $secondHash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash
    $after = (Invoke-AgentBusCacheCommand -Executable $SccachePath `
        -Arguments @('--show-stats', '--stats-format=json') -TimeoutSeconds $timeout -Operation stats-after).Output | ConvertFrom-Json
    $beforeHits = if ($before.stats.cache_hits.counts.PSObject.Properties['Rust']) {
        [long]$before.stats.cache_hits.counts.Rust
    } else { 0 }
    foreach ($counter in @('requests_not_cacheable', 'requests_unsupported_compiler',
            'cache_timeouts', 'compile_fails', 'cache_read_errors', 'cache_write_errors',
            'non_cacheable_compilations')) {
        if ($after.stats.$counter -ne $before.stats.$counter) {
            throw "Cache preflight failure counter changed: $counter"
        }
    }
    # Empty JSON counter objects have no member-enumerated Value in strict mode.
    $beforeErrors = 0L
    $afterErrors = 0L
    foreach ($property in $before.stats.cache_errors.counts.PSObject.Properties) { $beforeErrors += [long]$property.Value }
    foreach ($property in $after.stats.cache_errors.counts.PSObject.Properties) { $afterErrors += [long]$property.Value }
    if ($afterErrors -ne $beforeErrors) { throw 'Cache preflight cache error count changed' }
    $afterHits = if ($after.stats.cache_hits.counts.PSObject.Properties['Rust']) {
        [long]$after.stats.cache_hits.counts.Rust
    } else { 0 }
    if ($after.version -cne $Policy.version -or
        $after.stats.requests_executed -lt ($before.stats.requests_executed + 2) -or
        $afterHits -lt ($beforeHits + 1) -or $firstHash -cne $secondHash) {
        throw 'Cache preflight did not prove two executed requests, a Rust cache hit and identical artifacts'
    }
    $receipt = [ordered]@{
        schema_version = 1
        observed_utc = [DateTime]::UtcNow.ToString('o')
        server_port = $Policy.server_port
        version = $after.version
        executable_sha256 = (Get-FileHash -LiteralPath $SccachePath -Algorithm SHA256).Hash
        sccache_sha256 = (Get-FileHash -LiteralPath $SccachePath -Algorithm SHA256).Hash
        rustc_sha256 = (Get-FileHash -LiteralPath $RustcPath -Algorithm SHA256).Hash
        compilation_target = if ($TargetTriple) { $TargetTriple } else { 'host-default' }
        compilation_kind = if ($TargetTriple) { 'explicit-gnu-target' } else { 'host-default' }
        compiler_host_triple = 'NOT_ESTABLISHED'
        host_platform_differs_from_target_platform = [bool]($TargetTriple -and (-not [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([Runtime.InteropServices.OSPlatform]::Windows) -or [Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [Runtime.InteropServices.Architecture]::X64))
        host_os = [Runtime.InteropServices.RuntimeInformation]::OSDescription
        host_arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        preflight_directory = $preflightLeaf
        owner = $owner
        artifact_sha256 = $secondHash
        before = $before
        after = $after
        wrapper_fallback = $false
    }
    [IO.File]::WriteAllText((Join-Path $directory 'proof.json'), ($receipt | ConvertTo-Json -Depth 12),
        [Text.UTF8Encoding]::new($false))
    return $receipt
}
