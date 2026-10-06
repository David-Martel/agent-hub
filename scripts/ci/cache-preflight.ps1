#Requires -Version 7.4
Set-StrictMode -Version Latest

function Invoke-AgentBusCacheCommand {
    param([string]$Executable, [string[]]$Arguments, [int]$TimeoutSeconds)

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
    try {
        if (-not $process.Start()) { throw 'Could not start cache preflight client' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            # Kill only our exact client. Never stop a shared/daemon descendant.
            $process.Kill()
            $process.WaitForExit()
            throw 'Cache preflight client timed out; daemon left untouched'
        }
        $result = [pscustomobject]@{
            ExitCode = $process.ExitCode
            Output = $stdout.GetAwaiter().GetResult()
            Error = $stderr.GetAwaiter().GetResult()
        }
        if ($result.ExitCode -ne 0) {
            throw "Cache preflight client failed ($($result.ExitCode)): $($result.Error)"
        }
        return $result
    } finally {
        $process.Dispose()
    }
}

function Invoke-AgentBusCachePreflight {
    param([string]$SccachePath, [string]$RustcPath, [string]$TargetDirectory, $Policy)

    if ($Policy.schema_version -ne 1 -or $Policy.version -cne '0.18.0' -or
        $Policy.server_port -ne 4228 -or $Policy.command_timeout_seconds -lt 1 -or
        $Policy.command_timeout_seconds -gt 60 -or $Policy.preflight_directory -cne 'cache-preflight') {
        throw 'Invalid dedicated Windows cache policy'
    }
    $directory = Join-Path $TargetDirectory $Policy.preflight_directory
    $marker = Join-Path $directory 'owner.txt'
    $owner = 'agent-hub-ci-cache-preflight-v1'
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
        -Arguments @('--show-stats', '--stats-format=json') -TimeoutSeconds $timeout).Output | ConvertFrom-Json
    # --show-stats can return successful synthetic zero stats with NO daemon.
    # A serial actual compiler request starts an absent dedicated daemon before
    # Cargo fans out. Existing daemons are never restarted or reset here.
    $arguments = @($RustcPath, '--crate-name', 'agent_hub_cache_preflight', '--edition=2024',
        $source, '--crate-type=rlib', '--emit=link', '--out-dir', $directory, '-Dwarnings')
    $null = Invoke-AgentBusCacheCommand -Executable $SccachePath -Arguments $arguments -TimeoutSeconds $timeout
    $artifact = Join-Path $directory 'libagent_hub_cache_preflight.rlib'
    if (-not (Test-Path -LiteralPath $artifact -PathType Leaf) -or
        (Get-Item -LiteralPath $artifact).Length -eq 0) { throw 'Cache preflight produced no artifact' }
    $firstHash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash
    $null = Invoke-AgentBusCacheCommand -Executable $SccachePath -Arguments $arguments -TimeoutSeconds $timeout
    $secondHash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash
    $after = (Invoke-AgentBusCacheCommand -Executable $SccachePath `
        -Arguments @('--show-stats', '--stats-format=json') -TimeoutSeconds $timeout).Output | ConvertFrom-Json
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
        artifact_sha256 = $secondHash
        before = $before
        after = $after
        wrapper_fallback = $false
    }
    [IO.File]::WriteAllText((Join-Path $directory 'proof.json'), ($receipt | ConvertTo-Json -Depth 12),
        [Text.UTF8Encoding]::new($false))
    return $receipt
}
