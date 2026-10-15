[CmdletBinding()]
param([string]$SourcePath = (Join-Path $PSScriptRoot 'Wait-DisposableRedisReady.ps1'))

$ErrorActionPreference = 'Stop'
$resolvedSource = (Resolve-Path -LiteralPath $SourcePath).Path
. $resolvedSource
$results = [Collections.Generic.List[object]]::new()

# Only the external Redis peer is a fixture. The production helper, real TCP,
# byte parsing, retry loop, and total deadline execute unchanged.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
public sealed class DisposableRespRequest {
    public string Value { get; }
    public int ReadCalls { get; }
    public string Termination { get; }
    public DisposableRespRequest(string value, int reads, string termination) {
        Value = value; ReadCalls = reads; Termination = termination;
    }
}
public sealed class DisposableRespFixture : IDisposable {
    private readonly TcpListener listener;
    private readonly CancellationTokenSource stop = new CancellationTokenSource();
    public readonly ConcurrentQueue<DisposableRespRequest> Requests = new ConcurrentQueue<DisposableRespRequest>();
    public readonly Task Worker;
    public int Port { get; }
    public DisposableRespFixture(string mode, int startDelay) {
        listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start(); Port = ((IPEndPoint)listener.LocalEndpoint).Port;
        if (mode == "closed" || startDelay > 0) {
            listener.Stop();
            listener = new TcpListener(IPAddress.Loopback, Port);
        }
        Worker = Task.Run(async () => {
            try {
                if (mode == "closed") return;
                if (startDelay > 0) {
                    await Task.Delay(startDelay, stop.Token);
                    listener.Start();
                }
                while (!stop.IsCancellationRequested) {
                    using (var peer = await listener.AcceptTcpClientAsync(stop.Token)) {
                        using (var stream = peer.GetStream()) {
                            var request = new byte[14]; int count = 0, reads = 0;
                            string termination = "complete";
                            try {
                                while (count < request.Length) {
                                    // Force actual short reads in the existing fragmented case.
                                    int size = mode == "fragmented" ? 1 : request.Length - count;
                                    int n = await stream.ReadAsync(request.AsMemory(count, size), stop.Token);
                                    reads++;
                                    if (n == 0) { termination = "eof"; break; }
                                    count += n;
                                }
                            } catch (OperationCanceledException) when (stop.IsCancellationRequested) {
                                termination = "fixture-stop";
                            }
                            Requests.Enqueue(new DisposableRespRequest(
                                Encoding.ASCII.GetString(request, 0, count), reads, termination));
                            // An aborted connection is not a complete protocol frame and
                            // must not receive PONG. Keep its bytes for the settled oracle.
                            if (count != request.Length) continue;
                            if (mode == "eof") continue;
                            if (mode == "slow") await Task.Delay(5000, stop.Token);
                            var reply = Encoding.ASCII.GetBytes(mode == "wrong" ? "+NOPE\r\n" : "+PONG\r\n");
                            if (mode == "trickle" || mode == "fragmented") {
                                foreach (byte value in reply) {
                                    await stream.WriteAsync(new byte[]{value}, stop.Token);
                                    await Task.Delay(mode == "trickle" ? 400 : 10, stop.Token);
                                }
                            } else await stream.WriteAsync(reply, stop.Token);
                        }
                    }
                }
            } catch (OperationCanceledException) { }
              catch (SocketException) when (stop.IsCancellationRequested) { }
              catch (System.IO.IOException) when (stop.IsCancellationRequested || mode == "trickle") { }
        });
    }
    public void Dispose() {
        stop.Cancel(); listener.Stop();
        if (!Worker.Wait(2000)) throw new Exception("Owned RESP fixture did not stop");
        stop.Dispose();
    }
}
'@

function Assert-DisposableRespRequests {
    param([object[]]$Requests, [string]$CaseName, [bool]$AllowAborted = $false)
    $expected = "*1`r`n`$4`r`nPING`r`n"
    $complete = $aborted = 0
    foreach ($request in $Requests) {
        $length = $request.Value.Length
        if (-not $expected.StartsWith($request.Value, [StringComparison]::Ordinal)) {
            throw "Actual RESP request differs from PING (case=$CaseName; bytes=$length; reads=$($request.ReadCalls); termination=$($request.Termination))"
        }
        if ($length -eq $expected.Length -and $request.Termination -eq 'complete') { $complete++ }
        elseif ($AllowAborted -and $request.Termination -in @('eof', 'fixture-stop')) { $aborted++ }
        else {
            throw "Incomplete RESP frame outside a timed-out attempt (case=$CaseName; bytes=$length; reads=$($request.ReadCalls); termination=$($request.Termination))"
        }
    }
    if ($complete -lt 1) { throw "Fixture did not receive a complete actual TCP PING (case=$CaseName; aborted=$aborted)" }
    [pscustomobject]@{ complete = $complete; aborted = $aborted }
}

foreach ($case in @(
    @{ name = 'exact PONG'; mode = 'pong'; delay = 0; ready = $true },
    @{ name = 'fragmented PONG'; mode = 'fragmented'; delay = 0; ready = $true },
    @{ name = 'delayed listener'; mode = 'pong'; delay = 350; ready = $true },
    @{ name = 'closed port'; mode = 'closed'; delay = 0; ready = $false },
    @{ name = 'wrong protocol'; mode = 'wrong'; delay = 0; ready = $false },
    @{ name = 'EOF'; mode = 'eof'; delay = 0; ready = $false },
    @{ name = 'slow response'; mode = 'slow'; delay = 0; ready = $false },
    @{ name = 'trickle cannot extend deadline'; mode = 'trickle'; delay = 0; ready = $false }
)) {
    $peer = [DisposableRespFixture]::new($case.mode, $case.delay)
    try {
        $watch = [Diagnostics.Stopwatch]::StartNew()
        $failure = $null
        $output = @()
        try { $output = @(Wait-DisposableRedisReady -RedisUrl "redis://localhost:$($peer.Port)/0" -TimeoutSeconds 1) }
        catch { $failure = $_.Exception.Message }
        $elapsed = $watch.ElapsedMilliseconds
        if ($case.ready) {
            if ($failure -or $output.Count -ne 1 -or $output[0].Ready -isnot [bool] -or -not $output[0].Ready) {
                throw "Readiness singleton result failed: $($case.name)"
            }
            if ($case.delay -gt 0 -and $elapsed -lt $case.delay) { throw 'Delayed listener delay was not exercised' }
        }
        elseif (-not $failure -or $failure -notmatch '^Disposable Redis readiness timed out ' -or $output.Count -ne 0) {
            throw "Readiness did not fail closed: $($case.name)"
        }
        if ($elapsed -gt 2000 -or (-not $case.ready -and $elapsed -lt 900)) { throw "Total deadline violated: $($case.name)" }
    }
    finally { $peer.Dispose() }
    # Shutdown settles accepted streams before inspecting their final frames.
    $requests = $peer.Requests.ToArray()
    if ($case.mode -eq 'closed') {
        if ($requests.Count -ne 0) { throw 'Closed fixture accepted a connection' }
        $frames = [pscustomobject]@{ complete = 0; aborted = 0 }
    } else {
        $frames = Assert-DisposableRespRequests -Requests $requests -CaseName $case.name -AllowAborted (-not $case.ready)
        if ($case.mode -eq 'fragmented' -and @($requests | Where-Object { $_.ReadCalls -ge 14 }).Count -lt 1) {
            throw 'Actual fragmented request reads were not exercised'
        }
    }
    $results.Add([pscustomobject]@{ name = $case.name; passed = $true; elapsedMilliseconds = $elapsed;
        requests = $frames.complete; abortedConnections = $frames.aborted })
}

# Real TCP controls preserve cancellation evidence without accepting malformed
# bytes or losing the full-PING witness. These do not mock the readiness helper.
foreach ($control in @(
    @{ name = 'empty EOF retained as aborted'; value = ''; malformed = $false },
    @{ name = 'partial PING EOF retained as aborted'; value = "*1`r`n`$"; malformed = $false },
    @{ name = 'wrong partial frame still rejected'; value = '*2'; malformed = $true },
    @{ name = 'wrong complete frame still rejected'; value = "*1`r`n`$4`r`nPANG`r`n"; malformed = $true }
)) {
    $peer = [DisposableRespFixture]::new('pong', 0)
    try {
        $tcp = [Net.Sockets.TcpClient]::new()
        try {
            $connect = $tcp.ConnectAsync([Net.IPAddress]::Loopback, $peer.Port)
            if (-not $connect.Wait(1000)) { throw 'Owned frame control connect did not settle' }
            $connect.GetAwaiter().GetResult() | Out-Null
            $stream = $tcp.GetStream()
            $bytes = [Text.Encoding]::ASCII.GetBytes($control.value)
            if ($bytes.Length -gt 0) {
                $write = $stream.WriteAsync($bytes, 0, $bytes.Length)
                if (-not $write.Wait(1000)) { throw 'Owned frame control write did not settle' }
                $write.GetAwaiter().GetResult() | Out-Null
            }
            $tcp.Client.Shutdown([Net.Sockets.SocketShutdown]::Send)
            $read = $stream.ReadAsync([byte[]]::new(32), 0, 32)
            if (-not $read.Wait(1000)) { throw 'Owned frame control EOF did not settle' }
            [void]$read.GetAwaiter().GetResult()
        } finally { $tcp.Dispose() }
        $ready = Wait-DisposableRedisReady -RedisUrl "redis://localhost:$($peer.Port)/0" -TimeoutSeconds 1
        if (-not $ready.Ready) { throw 'Full PING control did not prove readiness' }
    } finally { $peer.Dispose() }
    $requests = $peer.Requests.ToArray()
    if ($requests.Count -ne 2 -or $requests[0].Value -cne $control.value) {
        throw 'Owned EOF bytes were not retained before the full PING witness'
    }
    $failure = $null
    try { $frames = Assert-DisposableRespRequests -Requests $requests -CaseName $control.name -AllowAborted $true }
    catch { $failure = $_.Exception.Message }
    if ($control.malformed) {
        if ($failure -notmatch '^Actual RESP request differs from PING ') { throw 'Wrong nonempty frame was accepted or misclassified' }
    } else {
        if ($failure -or $frames.complete -ne 1 -or $frames.aborted -ne 1) { throw 'Aborted/full frame accounting failed' }
        $strictFailure = $missingWitnessFailure = $null
        try { Assert-DisposableRespRequests -Requests $requests -CaseName 'successful-probe strict oracle' | Out-Null }
        catch { $strictFailure = $_.Exception.Message }
        try { Assert-DisposableRespRequests -Requests @($requests[0]) -CaseName 'missing full witness' -AllowAborted $true | Out-Null }
        catch { $missingWitnessFailure = $_.Exception.Message }
        if ($strictFailure -notmatch '^Incomplete RESP frame ' -or $missingWitnessFailure -notmatch '^Fixture did not receive a complete actual TCP PING ') {
            throw 'Successful probe or full-PING witness guard was weakened'
        }
    }
    $results.Add([pscustomobject]@{ name = $control.name; passed = $true;
        retainedBytes = $requests[0].Value.Length; fullWitnesses = 1 })
}
foreach ($value in @('', 'not-url', 'redis://remote.invalid:16382', 'rediss://localhost:16382',
    'redis://secret@localhost:16382', 'redis://localhost:16382?secret=value', 'redis://localhost:16382#secret',
    'redis://localhost', 'redis://localhost:1', 'redis://localhost:6380', 'redis://localhost:5300',
    'redis://localhost:8400', 'redis://localhost:18400')) {
    $failure = $null
    try { Wait-DisposableRedisReady -RedisUrl $value -TimeoutSeconds 1 | Out-Null }
    catch { $failure = $_.Exception.Message }
    if (-not $failure -or $failure -match 'secret|remote.invalid') { throw 'Unsafe URL was accepted or echoed' }
    $results.Add([pscustomobject]@{ name = 'unsafe URL rejected without disclosure'; passed = $true })
}
[pscustomobject]@{ sourceSha256 = (Get-FileHash -LiteralPath $resolvedSource).Hash;
    passed = $results.Count; failed = 0; skipped = 0; coreMocked = $false; cases = $results } | ConvertTo-Json -Depth 4
