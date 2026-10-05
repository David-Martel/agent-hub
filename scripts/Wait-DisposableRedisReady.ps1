<#
.SYNOPSIS
Waits for an actual RESP PONG from a disposable published loopback Redis port.
.DESCRIPTION
Uses one total deadline across bounded TCP connect, write, read, and poll attempts.
Rejects live bus ports, remote hosts, TLS, and credential-bearing URLs. Reports
only readiness metadata; it never prints URLs, response bytes, or credentials.
#>
[CmdletBinding()]
param(
    [string]$RedisUrl,
    [ValidateRange(1, 60)][int]$TimeoutSeconds = 30
)

function Invoke-DisposableRedisProbe {
    param([Net.IPAddress]$Address, [int]$Port, [Diagnostics.Stopwatch]$Clock, [long]$DeadlineMilliseconds)
    $tcp = [Net.Sockets.TcpClient]::new($Address.AddressFamily)
    $stream = $null
    $stage = 'connect'
    try {
        $connect = $tcp.ConnectAsync($Address, $Port)
        $remaining = [int][Math]::Max(0, $DeadlineMilliseconds - $Clock.ElapsedMilliseconds)
        if (-not $connect.Wait($remaining)) { return [pscustomobject]@{ Ready = $false; Stage = $stage } }
        $connect.GetAwaiter().GetResult() | Out-Null
        $stream = $tcp.GetStream()
        $stage = 'write'
        $request = [Text.Encoding]::ASCII.GetBytes("*1`r`n`$4`r`nPING`r`n")
        $write = $stream.WriteAsync($request, 0, $request.Length)
        $remaining = [int][Math]::Max(0, $DeadlineMilliseconds - $Clock.ElapsedMilliseconds)
        if (-not $write.Wait($remaining)) { return [pscustomobject]@{ Ready = $false; Stage = $stage } }
        $write.GetAwaiter().GetResult() | Out-Null
        $stage = 'read'
        $expected = [Text.Encoding]::ASCII.GetBytes("+PONG`r`n")
        $reply = [byte[]]::new($expected.Length)
        $offset = 0
        while ($offset -lt $reply.Length) {
            $remaining = [int][Math]::Max(0, $DeadlineMilliseconds - $Clock.ElapsedMilliseconds)
            if ($remaining -eq 0) { return [pscustomobject]@{ Ready = $false; Stage = $stage } }
            $read = $stream.ReadAsync($reply, $offset, $reply.Length - $offset)
            if (-not $read.Wait($remaining)) { return [pscustomobject]@{ Ready = $false; Stage = $stage } }
            $count = $read.GetAwaiter().GetResult()
            if ($count -eq 0) { return [pscustomobject]@{ Ready = $false; Stage = 'eof' } }
            for ($index = $offset; $index -lt ($offset + $count); $index++) {
                if ($reply[$index] -ne $expected[$index]) {
                    return [pscustomobject]@{ Ready = $false; Stage = 'protocol' }
                }
            }
            $offset += $count
        }
        return [pscustomobject]@{ Ready = ($Clock.ElapsedMilliseconds -lt $DeadlineMilliseconds); Stage = 'pong' }
    }
    catch {
        # Socket/Task exception text can contain endpoint data. Only the fixed
        # phase is returned; every unsuccessful attempt remains fail-closed.
        return [pscustomobject]@{ Ready = $false; Stage = $stage }
    }
    finally {
        if ($stream) { $stream.Dispose() }
        $tcp.Dispose()
    }
}

function Wait-DisposableRedisReady {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$RedisUrl,
        [ValidateRange(1, 60)][int]$TimeoutSeconds = 30
    )
    $target = $null
    if ([string]::IsNullOrWhiteSpace($RedisUrl) -or
        -not [uri]::TryCreate($RedisUrl, [UriKind]::Absolute, [ref]$target) -or
        $target.Scheme -ne 'redis' -or $target.UserInfo -or $target.Query -or $target.Fragment -or
        $target.Host.Trim('[', ']') -notin @('localhost', '127.0.0.1', '::1') -or
        $target.Port -lt 1024 -or $target.Port -in @(6380, 5300, 8400, 18400)) {
        throw 'Redis readiness requires a credential-free disposable loopback Redis URL on a non-live port.'
    }
    # Docker publishes to IPv4 loopback in this workflow; localhost is explicit
    # IPv4 here rather than a DNS-dependent IPv6-first connection sequence.
    $address = if ($target.Host.Trim('[', ']') -eq '::1') { [Net.IPAddress]::IPv6Loopback } else { [Net.IPAddress]::Loopback }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $deadline = [long]$TimeoutSeconds * 1000
    $attempts = 0
    $lastStage = 'connect'
    while ($clock.ElapsedMilliseconds -lt $deadline) {
        $attempts++
        $attemptDeadline = [Math]::Min($deadline, $clock.ElapsedMilliseconds + 1500)
        $probe = Invoke-DisposableRedisProbe -Address $address -Port $target.Port -Clock $clock -DeadlineMilliseconds $attemptDeadline
        $lastStage = $probe.Stage
        if ($probe.Ready) {
            return [pscustomobject]@{ Ready = $true; Attempts = $attempts; ElapsedMilliseconds = $clock.ElapsedMilliseconds }
        }
        $remaining = $deadline - $clock.ElapsedMilliseconds
        if ($remaining -gt 0) { Start-Sleep -Milliseconds ([Math]::Min(200, $remaining)) }
    }
    throw "Disposable Redis readiness timed out (attempts=$attempts; phase=$lastStage; deadline_seconds=$TimeoutSeconds)."
}

if ($MyInvocation.InvocationName -ne '.') {
    Wait-DisposableRedisReady -RedisUrl $RedisUrl -TimeoutSeconds $TimeoutSeconds
}
