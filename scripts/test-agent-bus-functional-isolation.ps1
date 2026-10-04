[CmdletBinding()]
param(
    [string]$SourcePath = (Join-Path $PSScriptRoot 'test-agent-bus-functional.ps1')
)

$ErrorActionPreference = 'Stop'
$environmentKeys = @('AGENT_BUS_CONFIG', 'AGENT_BUS_REDIS_URL', 'AGENT_BUS_DATABASE_URL',
    'AGENT_BUS_SERVER_HOST', 'AGENT_BUS_SERVER_URL', 'AGENT_BUS_SERVER_URLS',
    'AGENT_BUS_SERVER_CANDIDATES', 'AGENT_BUS_AUTH_TOKEN', 'AGENT_BUS_STARTUP_ENABLED',
    'AGENT_BUS_HUB_CACHE_TTL_SECONDS')
$fixtureKeys = @('PCAI_SMOKE_FIXTURE_EVENTS', 'PCAI_SMOKE_FIXTURE_FAILURE', 'GITHUB_STEP_SUMMARY')
$savedEnvironment = @{}
foreach ($key in $environmentKeys + $fixtureKeys) {
    $savedEnvironment[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
}
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('agent-bus-smoke-isolation-' + [guid]::NewGuid().ToString('N'))
$results = [Collections.Generic.List[object]]::new()

function Assert-Fixture {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Invoke-FixtureCase {
    param(
        [string]$Name,
        [hashtable]$Overrides = @{},
        [string]$Failure = '',
        [switch]$ExpectFailure,
        [switch]$RejectBeforeCli,
        [switch]$AbsentOriginals
    )

    $eventsPath = Join-Path $fixtureRoot 'events.jsonl'
    [IO.File]::WriteAllText($eventsPath, '')
    $originals = @{}
    foreach ($key in $environmentKeys) {
        if ($AbsentOriginals) {
            Remove-Item -LiteralPath ('Env:' + $key) -ErrorAction SilentlyContinue
        }
        else {
            [Environment]::SetEnvironmentVariable($key, 'synthetic-original-' + $key, 'Process')
        }
    }
    if (-not $AbsentOriginals) {
        $env:AGENT_BUS_CONFIG = Join-Path $fixtureRoot 'original-config.json'
        $env:AGENT_BUS_SERVER_URL = 'http://synthetic-single.invalid:8400'
        $env:AGENT_BUS_SERVER_URLS = 'http://synthetic-list.invalid:8400'
        $env:AGENT_BUS_SERVER_CANDIDATES = '[{"url":"https://synthetic-cloud.invalid","token_env":"SYNTHETIC_TOKEN"}]'
        $env:AGENT_BUS_AUTH_TOKEN = 'synthetic-original-private-token'
    }
    foreach ($key in $environmentKeys) {
        $originals[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
    }
    $env:PCAI_SMOKE_FIXTURE_EVENTS = $eventsPath
    $env:PCAI_SMOKE_FIXTURE_FAILURE = $Failure
    Remove-Item -LiteralPath Env:GITHUB_STEP_SUMMARY -ErrorAction SilentlyContinue
    $arguments = @{
        CliPath = (Join-Path $fixtureRoot 'fake-cli.ps1')
        HttpBinaryPath = (Join-Path $fixtureRoot 'fake-http.ps1')
        RedisUrl = 'redis://127.0.0.1:16382/0'
        DatabaseUrl = 'postgresql://postgres@127.0.0.1:15302/fixture'
        HttpPort = 18410
        HttpAuthToken = 'synthetic-disposable-http-token'
    }
    foreach ($key in $Overrides.Keys) { $arguments[$key] = $Overrides[$key] }
    $failureMessage = $null
    try { & (Join-Path $fixtureRoot 'test-agent-bus-functional.ps1') @arguments *> $null }
    catch { $failureMessage = $_.Exception.Message }
    Assert-Fixture -Condition (($null -ne $failureMessage) -eq [bool]$ExpectFailure) -Message "$Name unexpected outcome: $failureMessage"
    $events = @(Get-Content -LiteralPath $eventsPath | ForEach-Object { $_ | ConvertFrom-Json })
    if ($RejectBeforeCli) {
        Assert-Fixture -Condition ($events.Count -eq 0) -Message "$Name invoked external CLI before rejecting unsafe input"
        Assert-Fixture -Condition ($failureMessage -match 'must (explicitly identify|identify)') -Message "$Name failed for the wrong reason: $failureMessage"
    }
    else {
        Assert-Fixture -Condition ($events.Count -ge 1) -Message "$Name never exercised the production smoke entrypoint"
        Assert-Fixture -Condition ($events[0].kind -eq 'health') -Message "$Name did not start with real source health dispatch"
        foreach ($ioRecord in $events) {
            $snapshot = $ioRecord.environment
            foreach ($key in @('AGENT_BUS_SERVER_URL', 'AGENT_BUS_SERVER_URLS', 'AGENT_BUS_SERVER_CANDIDATES')) {
                Assert-Fixture -Condition ($null -eq $snapshot.$key) -Message "$Name retained $key in $($ioRecord.kind)"
            }
            Assert-Fixture -Condition ($snapshot.AGENT_BUS_SERVER_HOST -eq 'localhost') -Message "$Name retained remote listener binding"
            Assert-Fixture -Condition ($snapshot.AGENT_BUS_STARTUP_ENABLED -eq 'false') -Message "$Name enabled startup broadcasts"
            Assert-Fixture -Condition ($snapshot.AGENT_BUS_HUB_CACHE_TTL_SECONDS -eq '0') -Message "$Name retained shared discovery cache"
            Assert-Fixture -Condition ($snapshot.AGENT_BUS_REDIS_URL -eq $arguments.RedisUrl) -Message "$Name lost disposable Redis URL"
            $expectedDatabase = if ($ioRecord.mode -eq 'Degraded') { 'postgresql://postgres@127.0.0.1:1/redis_backend' } else { $arguments.DatabaseUrl }
            Assert-Fixture -Condition ($snapshot.AGENT_BUS_DATABASE_URL -eq $expectedDatabase) -Message "$Name used wrong database in $($ioRecord.kind)/$($ioRecord.mode)"
            Assert-Fixture -Condition $ioRecord.configExists -Message "$Name config missing during I/O"
            Assert-Fixture -Condition ($ioRecord.configBody -eq '{}') -Message "$Name did not isolate private configuration"
            Assert-Fixture -Condition ($snapshot.AGENT_BUS_CONFIG -ne $originals.AGENT_BUS_CONFIG) -Message "$Name reused original config"
            $token = $snapshot.AGENT_BUS_AUTH_TOKEN
            if ($arguments.HttpAuthToken) {
                Assert-Fixture -Condition ($token -eq $arguments.HttpAuthToken) -Message "$Name inherited private auth token"
            }
            else {
                Assert-Fixture -Condition ($token -match '^[a-f0-9]{32}$') -Message "$Name did not generate disposable auth token"
            }
            if ($ioRecord.kind -eq 'http') {
                Assert-Fixture -Condition ($ioRecord.tokenParameter -eq $token) -Message "$Name HTTP argument/env tokens differ"
                $expectedPort = if ($ioRecord.mode -eq 'Degraded') { $arguments.HttpPort + 1 } else { $arguments.HttpPort }
                Assert-Fixture -Condition ($ioRecord.port -eq $expectedPort) -Message "$Name wrong HTTP port"
                Assert-Fixture -Condition ($ioRecord.baseUrl -eq "http://localhost:$expectedPort") -Message "$Name HTTP URL is not isolated loopback"
            }
        }
        $configPaths = @($events | ForEach-Object { $_.environment.AGENT_BUS_CONFIG } | Select-Object -Unique)
        Assert-Fixture -Condition ($configPaths.Count -eq 1) -Message "$Name changed owned config mid-run"
        Assert-Fixture -Condition (-not (Test-Path -LiteralPath $configPaths[0])) -Message "$Name leaked owned config after completion/failure"
        if (-not $ExpectFailure) {
            Assert-Fixture -Condition ($events.Count -eq 5) -Message "$Name omitted a normal or forced-degraded smoke"
            Assert-Fixture -Condition (($events.kind -join ',') -eq 'health,cli,http,cli,http') -Message "$Name wrong smoke sequence"
            Assert-Fixture -Condition (($events.mode -join ',') -eq 'health,Healthy,Healthy,Degraded,Degraded') -Message "$Name wrong database mode sequence"
        }
        else {
            $expectedKinds = switch ($Failure) {
                { $_ -in @('health-exit', 'health-not-ok', 'health-postgres') } { 'health' }
                'cli-smoke' { 'health,cli' }
                'http-smoke' { 'health,cli,http' }
                'forced-cli' { 'health,cli,http,cli' }
                'forced-http' { 'health,cli,http,cli,http' }
                default { throw "Unknown fixture failure: $Failure" }
            }
            Assert-Fixture -Condition (($events.kind -join ',') -eq $expectedKinds) -Message "$Name failure happened at the wrong phase"
        }
    }
    foreach ($key in $environmentKeys) {
        $actual = [Environment]::GetEnvironmentVariable($key, 'Process')
        Assert-Fixture -Condition ($actual -ceq $originals[$key]) -Message "$Name did not restore exact $key presence/value"
    }
    Assert-Fixture -Condition ((Get-Content -LiteralPath (Join-Path $fixtureRoot 'original-config.json') -Raw) -eq '{"syntheticOriginal":true}') -Message "$Name changed or removed original config"
    $results.Add([pscustomobject]@{ name = $Name; passed = $true; ioEvents = $events.Count })
}

try {
    New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
    $sourceBytes = [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $SourcePath).Path)
    $sourceHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($sourceBytes))
    [IO.File]::WriteAllBytes((Join-Path $fixtureRoot 'test-agent-bus-functional.ps1'), $sourceBytes)
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'original-config.json'), '{"syntheticOriginal":true}')
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'fixture-capture.ps1'), @'
function Write-FixtureEvent {
    param([string]$Kind, [string]$Mode, [string]$TokenParameter, [string]$BaseUrl, [int]$Port)
    $keys = @('AGENT_BUS_CONFIG', 'AGENT_BUS_REDIS_URL', 'AGENT_BUS_DATABASE_URL',
        'AGENT_BUS_SERVER_HOST', 'AGENT_BUS_SERVER_URL', 'AGENT_BUS_SERVER_URLS',
        'AGENT_BUS_SERVER_CANDIDATES', 'AGENT_BUS_AUTH_TOKEN', 'AGENT_BUS_STARTUP_ENABLED',
        'AGENT_BUS_HUB_CACHE_TTL_SECONDS')
    $environment = @{}
    foreach ($key in $keys) { $environment[$key] = [Environment]::GetEnvironmentVariable($key, 'Process') }
    $configExists = Test-Path -LiteralPath $env:AGENT_BUS_CONFIG
    $configBody = if ($configExists) { Get-Content -LiteralPath $env:AGENT_BUS_CONFIG -Raw } else { $null }
    $ioRecord = @{ kind = $Kind; mode = $Mode; environment = $environment; configExists = $configExists;
        configBody = $configBody; tokenParameter = $TokenParameter; baseUrl = $BaseUrl; port = $Port }
    [IO.File]::AppendAllText($env:PCAI_SMOKE_FIXTURE_EVENTS, ($ioRecord | ConvertTo-Json -Depth 4 -Compress) + [Environment]::NewLine)
}
'@)
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'fake-cli.ps1'), @'
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$CommandArguments)
. (Join-Path $PSScriptRoot 'fixture-capture.ps1')
if (($CommandArguments -join ',') -ne 'health,--encoding,json') { throw 'Unexpected CLI arguments' }
Write-FixtureEvent -Kind health -Mode health
$global:LASTEXITCODE = if ($env:PCAI_SMOKE_FIXTURE_FAILURE -eq 'health-exit') { 7 } else { 0 }
@{ ok = ($env:PCAI_SMOKE_FIXTURE_FAILURE -ne 'health-not-ok');
    database_ok = ($env:PCAI_SMOKE_FIXTURE_FAILURE -ne 'health-postgres') } | ConvertTo-Json -Compress
'@)
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'fake-http.ps1'), "throw 'HTTP binary must only be passed to the fake smoke sibling'")
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'test-agent-bus-cli-smoke.ps1'), @'
param([string]$CliPath, [string]$DatabaseMode)
. (Join-Path $PSScriptRoot 'fixture-capture.ps1')
if (-not (Test-Path -LiteralPath $CliPath)) { throw 'Missing fake CLI path' }
Write-FixtureEvent -Kind cli -Mode $DatabaseMode
if ($env:PCAI_SMOKE_FIXTURE_FAILURE -eq 'cli-smoke' -or
    ($env:PCAI_SMOKE_FIXTURE_FAILURE -eq 'forced-cli' -and $DatabaseMode -eq 'Degraded')) { throw 'synthetic CLI smoke failure' }
'@)
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'test-agent-bus-http-smoke.ps1'), @'
param([string]$BinaryPath, [string]$BaseUrl, [int]$Port, [string]$DatabaseMode,
    [string]$AuthToken, [int]$StartupTimeoutSeconds)
. (Join-Path $PSScriptRoot 'fixture-capture.ps1')
if (-not (Test-Path -LiteralPath $BinaryPath)) { throw 'Missing fake HTTP path' }
if ($DatabaseMode -eq 'Degraded' -and $StartupTimeoutSeconds -ne 90) { throw 'Missing degraded startup timeout' }
Write-FixtureEvent -Kind http -Mode $DatabaseMode -TokenParameter $AuthToken -BaseUrl $BaseUrl -Port $Port
if ($env:PCAI_SMOKE_FIXTURE_FAILURE -eq 'http-smoke' -or
    ($env:PCAI_SMOKE_FIXTURE_FAILURE -eq 'forced-http' -and $DatabaseMode -eq 'Degraded')) { throw 'synthetic HTTP smoke failure' }
'@)

    foreach ($key in @('RedisUrl', 'DatabaseUrl')) {
        foreach ($value in @('', 'not-a-url', 'https://localhost:16382', 'redis://remote.invalid:16382/0')) {
            Invoke-FixtureCase -Name "$key rejected [$value]" -Overrides @{ $key = $value } -ExpectFailure -RejectBeforeCli
        }
    }
    foreach ($port in @(6380, 5300, 8400, 18400)) {
        Invoke-FixtureCase -Name "Redis live port $port" -Overrides @{ RedisUrl = "redis://localhost:$port/0" } -ExpectFailure -RejectBeforeCli
        Invoke-FixtureCase -Name "Postgres live port $port" -Overrides @{ DatabaseUrl = "postgresql://postgres@localhost:$port/fixture" } -ExpectFailure -RejectBeforeCli
        Invoke-FixtureCase -Name "HTTP live port $port" -Overrides @{ HttpPort = $port } -ExpectFailure -RejectBeforeCli
        Invoke-FixtureCase -Name "HTTP forced adjacent live port $port" -Overrides @{ HttpPort = $port - 1 } -ExpectFailure -RejectBeforeCli
    }
    foreach ($port in @(0, 1023, 65535)) {
        Invoke-FixtureCase -Name "HTTP invalid port $port" -Overrides @{ HttpPort = $port } -ExpectFailure -RejectBeforeCli
    }
    foreach ($case in @(
        @{ name = 'Redis query'; values = @{ RedisUrl = 'redis://localhost:16382/0?host=remote.invalid' } },
        @{ name = 'Postgres fragment'; values = @{ DatabaseUrl = 'postgresql://localhost:15302/fixture#remote' } },
        @{ name = 'Redis missing port'; values = @{ RedisUrl = 'redis://localhost/0' } },
        @{ name = 'Postgres remote host'; values = @{ DatabaseUrl = 'postgresql://postgres@remote.invalid:15302/fixture' } }
    )) {
        Invoke-FixtureCase -Name $case.name -Overrides $case.values -ExpectFailure -RejectBeforeCli
    }
    Invoke-FixtureCase -Name 'success synthetic original environment'
    Invoke-FixtureCase -Name 'success absent original environment generated token' -AbsentOriginals -Overrides @{ HttpAuthToken = '' }
    foreach ($failure in @('health-exit', 'health-not-ok', 'cli-smoke', 'http-smoke', 'forced-cli', 'forced-http')) {
        Invoke-FixtureCase -Name "cleanup after $failure" -Failure $failure -ExpectFailure
    }
    Invoke-FixtureCase -Name 'cleanup after required Postgres unhealthy' -Failure health-postgres -Overrides @{ RequirePostgres = $true } -ExpectFailure
    [pscustomobject]@{ sourceSha256 = $sourceHash; passed = $results.Count; failed = 0; cases = $results } | ConvertTo-Json -Depth 4
}
finally {
    foreach ($key in $environmentKeys + $fixtureKeys) {
        if ($null -eq $savedEnvironment[$key]) {
            Remove-Item -LiteralPath ('Env:' + $key) -ErrorAction SilentlyContinue
        }
        else {
            [Environment]::SetEnvironmentVariable($key, $savedEnvironment[$key], 'Process')
        }
    }
    $resolvedRoot = [IO.Path]::GetFullPath($fixtureRoot)
    $resolvedTemp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedRoot.StartsWith($resolvedTemp, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path $resolvedRoot -Leaf) -notmatch '^agent-bus-smoke-isolation-[a-f0-9]{32}$') {
        throw 'Refusing fixture cleanup outside the owned temporary directory'
    }
    if (Test-Path -LiteralPath $resolvedRoot) { Remove-Item -LiteralPath $resolvedRoot -Recurse -Force }
}
