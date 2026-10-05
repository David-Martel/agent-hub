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
public sealed class DisposableRespFixture : IDisposable {
    private readonly TcpListener listener;
    private readonly CancellationTokenSource stop = new CancellationTokenSource();
    public readonly ConcurrentQueue<string> Requests = new ConcurrentQueue<string>();
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
                            var request = new byte[14]; int count = 0;
                            while (count < request.Length) {
                                int n = await stream.ReadAsync(request.AsMemory(count), stop.Token);
                                if (n == 0) break;
                                count += n;
                            }
                            Requests.Enqueue(Encoding.ASCII.GetString(request, 0, count));
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
        $requests = $peer.Requests.ToArray()
        if ($case.mode -ne 'closed' -and $requests.Count -lt 1) { throw 'Fixture did not receive actual TCP PING' }
        foreach ($request in $requests) {
            if ($request -cne "*1`r`n`$4`r`nPING`r`n") { throw 'Actual RESP request differs from PING' }
        }
        $results.Add([pscustomobject]@{ name = $case.name; passed = $true; elapsedMilliseconds = $elapsed; requests = $requests.Count })
    }
    finally { $peer.Dispose() }
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
