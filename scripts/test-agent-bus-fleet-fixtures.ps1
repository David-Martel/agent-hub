$ErrorActionPreference = "Stop"

$doctor = Join-Path $PSScriptRoot "test-agent-bus-fleet.ps1"
$manifest = Join-Path (Split-Path -Parent $PSScriptRoot) "config/fleet/agent-bus-fleet-v1.json"

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "agent-bus-fleet-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
try {
. $doctor -ManifestPath $manifest -SkipLive -Strict | Out-Null

if (-not (Test-BuildRevisionMatch -VersionText "agent-bus 0.5.0 (v0.5.0-20-gfd70c8d)" -Revision "fd70c8d")) {
    throw "Fleet doctor rejected an exact clean build revision."
}
if (Test-BuildRevisionMatch -VersionText "agent-bus 0.5.0 (v0.5.0-20-gfd70c8d-dirty)" -Revision "fd70c8d") {
    throw "Fleet doctor accepted a dirty build revision."
}
if (Test-BuildRevisionMatch -VersionText "agent-bus 0.5.0 (v0.5.0-20-gfd70c8d-extra)" -Revision "fd70c8d") {
    throw "Fleet doctor accepted a non-exact build revision."
}

$fullRevision = "53292d9d706217994d1791227d60be6ed1f7f79f"
$installedRevision = '1233662bd4537bd2b2f7e1cfc034c40419ed4d21'
if (-not (Test-BuildRevisionMatch -VersionText "agent-bus 0.5.0 ($installedRevision 2026-10-10)" -Revision $installedRevision)) {
    throw 'Fleet doctor rejected the installed full-revision version format'
}
foreach ($version in @(
        "agent-bus 0.5.0 ($installedRevision-dirty 2026-10-10)",
        "agent-bus 0.5.0 ($installedRevision 2026-10-10) trailing",
        'agent-bus 0.5.0 (2233662bd4537bd2b2f7e1cfc034c40419ed4d21 2026-10-10)')) {
    if (Test-BuildRevisionMatch -VersionText $version -Revision $installedRevision) {
        throw 'Fleet doctor accepted dirty, trailing or wrong installed provenance'
    }
}
foreach ($version in @(
        "agent-bus 0.5.0 ($fullRevision 2026-10-08)",
        "0.5.0 ($fullRevision 2026-10-08)",
        "agent-bus-http 0.5.0 ($fullRevision 2026-10-08)",
        "0.5.0 (v0.5.0-20-g$fullRevision 2026-10-08)")) {
    if (-not (Test-BuildRevisionMatch -VersionText $version -Revision $fullRevision)) {
        throw "Fleet doctor rejected a supported clean provenance format: $version"
    }
}
foreach ($version in @(
        "agent-bus 0.5.0 ($fullRevision-dirty 2026-10-08)",
        "agent-bus 0.5.0 ($fullRevision-extra 2026-10-08)",
        "agent-bus 0.5.0 ($fullRevision 2026-10-08) ignored",
        "agent-bus 0.5.0 (53292d9 2026-10-08)",
        "agent-bus 0.5.0 (v0.5.0-20-g53292d9 2026-10-08)",
        "agent-bus 0.5.0 (63292d9d706217994d1791227d60be6ed1f7f79f 2026-10-08)")) {
    if (Test-BuildRevisionMatch -VersionText $version -Revision $fullRevision) {
        throw "Fleet doctor accepted dirty, suffixed, truncated or mismatched provenance: $version"
    }
}
if (Test-BuildRevisionMatch -VersionText "0.5.0 ($fullRevision 2026-10-08)" -Revision "53292d9") {
    throw "Fleet doctor accepted a revision prefix collision."
}

$fixtureMachine = [pscustomobject]@{
    id = "client-a"; role = "client"; client_server_url = "http://authority.invalid:8400"
}
$healthy = [pscustomobject]@{
    ok = $true; storage_ready = $true; protocol_version = "1.0"
    # The remote hub's provenance must win over client/local provenance.
    build_version = "0.5.0 (63292d9d706217994d1791227d60be6ed1f7f79f 2026-10-08)"
    hub_identity = "authority-a"; pg_dropped_writes = 0; pg_write_errors = 0
    backend = [pscustomobject]@{
        mode = "remote"; url = "http://authority.invalid:8400"; authoritative = $true
        hub_build = "0.5.0 ($fullRevision 2026-10-08)"
    }
}
$results.Clear()

$authorityFixture = [pscustomobject]@{
    id = "authority-a"; role = "authority"; allow_default_server_url = $true
    client_server_url = "http://localhost:8400"
}
$localHealth = $healthy | ConvertTo-Json -Depth 6 | ConvertFrom-Json
$localHealth.backend = [pscustomobject]@{ mode = "local" }
Test-FleetHealthRoute -Machine $authorityFixture -Health $localHealth -AuthorityMachine "authority-a"
if (@($results | Where-Object status -ne "ok").Count) {
    throw "Fleet doctor rejected the authority's local backend."
}
$results.Clear()
Test-FleetHealthRoute -Machine $fixtureMachine -Health $localHealth -AuthorityMachine "authority-a"
if (@($results | Where-Object { $_.check -eq "route" -and $_.status -eq "fail" }).Count -ne 1) {
    throw "Fleet doctor accepted a client-local island as the fleet route."
}
$results.Clear()
Test-FleetHealthRoute -Machine $fixtureMachine -Health $healthy -AuthorityMachine "authority-a"
Test-HealthDocument -Machine "client-a" -Health $healthy -ProtocolVersion "1.0" -Revision $fullRevision
if ($results.Count -ne 7 -or @($results | Where-Object status -ne "ok").Count) {
    throw "Fleet doctor rejected current remote health or selected the client's build."
}
foreach ($control in @("route", "hub-identity", "authority", "service-build-revision")) {
    $badHealth = $healthy | ConvertTo-Json -Depth 6 | ConvertFrom-Json
    switch ($control) {
        "route" { $badHealth.backend.url = "http://wrong.invalid:8400" }
        "hub-identity" { $badHealth.hub_identity = "other-hub" }
        "authority" { $badHealth.backend.authoritative = $false }
        "service-build-revision" { $badHealth.backend.hub_build = "0.5.0 (63292d9d706217994d1791227d60be6ed1f7f79f 2026-10-08)" }
    }
    $results.Clear()
    Test-FleetHealthRoute -Machine $fixtureMachine -Health $badHealth -AuthorityMachine "authority-a"
    Test-HealthDocument -Machine "client-a" -Health $badHealth -ProtocolVersion "1.0" -Revision $fullRevision
    if (@($results | Where-Object { $_.check -eq $control -and $_.status -eq "fail" }).Count -ne 1) {
        throw "Fleet doctor missed the $control negative control."
    }
}
$results.Clear()

$expectedLiteral = ConvertTo-PosixShellLiteral -Path "/opt/agent-bus/bin/agent-bus"
if ($expectedLiteral -ne "'/opt/agent-bus/bin/agent-bus'") {
    throw "Fleet doctor did not preserve a safe manifest path."
}
$unsafePathRejected = $false
try {
    ConvertTo-PosixShellLiteral -Path "/opt/agent-bus;echo-injected" | Out-Null
}
catch {
    if ($_.Exception.Message -ne "Unsafe absolute POSIX path in fleet manifest: /opt/agent-bus;echo-injected") { throw }
    $unsafePathRejected = $true
}
if (-not $unsafePathRejected) {
    throw "Fleet doctor accepted an unsafe remote manifest path."
}

    $tokenPath = Join-Path $fixtureRoot "candidate-token"
    # Empty and missing files do not establish a source. Metadata is sufficient;
    # the doctor must never consume or expose the fixture credential contents.
    [IO.File]::WriteAllText($tokenPath, "")
    $candidateConfig = [pscustomobject]@{
        auth_token_present = $true
        server_urls = @([pscustomobject]@{ url = "http://authority.invalid:8400"; token_file = $tokenPath })
    }
    if (Test-FleetConfigAuthSource -Config $candidateConfig -RouteUrl "http://authority.invalid:8400") {
        throw "Fleet doctor substituted global auth for an empty candidate token file."
    }
    [IO.File]::WriteAllText($tokenPath, "fixture-only")
    $candidateConfig.auth_token_present = $false
    if (-not (Test-FleetConfigAuthSource -Config $candidateConfig -RouteUrl "http://authority.invalid:8400")) {
        throw "Fleet doctor rejected an available candidate token-file source."
    }
    if (Test-FleetConfigAuthSource -Config $candidateConfig -RouteUrl "http://other.invalid:8400") {
        throw "Fleet doctor borrowed another route's token file."
    }
    $candidateConfig.server_urls[0].token_file = Join-Path $fixtureRoot "missing-token"
    $candidateConfig.auth_token_present = $true
    if (Test-FleetConfigAuthSource -Config $candidateConfig -RouteUrl "http://authority.invalid:8400") {
        throw "Fleet doctor substituted global auth for a missing candidate file."
    }
    $candidateConfig.server_urls[0].token_file = $tokenPath
    $candidateConfig.auth_token_present = $false
    $legacyConfig = [pscustomobject]@{ auth_token = "fixture-only"; server_url = "http://authority.invalid:8400" }
    if (-not (Test-FleetConfigAuthSource -Config $legacyConfig -RouteUrl "http://authority.invalid:8400")) {
        throw "Fleet doctor rejected the legacy global credential source."
    }

    $cliPath = Join-Path $fixtureRoot "fixture-cli.ps1"
    @'
if ($env:AGENT_BUS_SERVER_URL -cne "http://caller-route.invalid:8400") {
    throw "Fleet doctor changed caller routing during the health probe"
}
if ($args[0] -eq "--version") {
    Write-Output "agent-bus 0.5.0 (53292d9d706217994d1791227d60be6ed1f7f79f 2026-10-08)"
} elseif (($args -join " ") -eq "health --encoding json") {
    Write-Output '{"ok":true,"storage_ready":true,"protocol_version":"1.0","build_version":"0.5.0 (53292d9d706217994d1791227d60be6ed1f7f79f 2026-10-08)","hub_identity":"authority-a","pg_dropped_writes":0,"pg_write_errors":0,"backend":{"mode":"remote","url":"http://authority.invalid:8400","authoritative":true,"hub_build":"0.5.0 (53292d9d706217994d1791227d60be6ed1f7f79f 2026-10-08)"}}'
} else { throw "Unexpected fixture CLI command" }
$global:LASTEXITCODE = 0
'@ | Set-Content -LiteralPath $cliPath -Encoding utf8
    $configPath = Join-Path $fixtureRoot "candidate-config.json"
    $candidateConfig | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $configPath -Encoding utf8
    $liveFixturePath = Join-Path $fixtureRoot "synthetic-live.json"
    @{
        schema_version = 1; authority_machine = "authority-a"; expected_protocol_version = "1.0"
        expected_build_revision = $fullRevision
        machines = @(@{
            id = "authority-a"; connection = "local-windows"; os = "windows"; architecture = "x86_64"
            role = "authority"; canonical_repo = $fixtureRoot; cli_path = $cliPath; config_path = $configPath
            auth_source = "client-config"; client_server_url = "http://authority.invalid:8400"
            required_active_services = @(); required_inactive_services = @()
        })
    } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $liveFixturePath -Encoding utf8
    $previousUrl = $env:AGENT_BUS_SERVER_URL
    try {
        $env:AGENT_BUS_SERVER_URL = "http://caller-route.invalid:8400"
        $report = (& $doctor -ManifestPath $liveFixturePath -Strict -Json | Out-String) | ConvertFrom-Json
        if (@($report | Where-Object status -ne "ok").Count -or @($report | Where-Object check -eq "route").Count -ne 1) {
            throw "Synthetic live doctor did not qualify the candidate route."
        }
    } finally {
        if ($null -eq $previousUrl) { Remove-Item Env:AGENT_BUS_SERVER_URL -ErrorAction SilentlyContinue }
        else { $env:AGENT_BUS_SERVER_URL = $previousUrl }
    }

$doctorText = Get-Content -LiteralPath $doctor -Raw
foreach ($hardCodedPath in @('$HOME/.local/bin/agent-bus', '$HOME/.config/agent-bus/config.json')) {
    if ($doctorText.Contains($hardCodedPath, [System.StringComparison]::Ordinal)) {
        throw "Fleet doctor contains a hard-coded remote deployment path: $hardCodedPath"
    }
}

    $duplicatePath = Join-Path $fixtureRoot "duplicate.json"
    @{
        schema_version          = 1
        authority_machine       = "node-a"
        expected_build_revision = "fd70c8d"
        machines                = @(
            @{
                id = "node-a"; connection = "ssh-linux"; ssh_host = "node-a"; os = "linux"
                architecture = "x86_64"; role = "authority"; canonical_repo = "/repo"
                cli_path = "/bin/agent-bus"; config_path = "/config/agent-bus.json"
                auth_source = "hub-env"
                client_server_url = "http://node-a:8400"
            },
            @{
                id = "node-a"; connection = "ssh-linux"; ssh_host = "node-b"; os = "linux"
                architecture = "x86_64"; role = "client"; canonical_repo = "/repo"
                cli_path = "/bin/agent-bus"; config_path = "/config/agent-bus.json"
                auth_source = "client-config"
                client_server_url = "http://node-a:8400"
            }
        )
    } | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $duplicatePath -Encoding utf8

    $rejected = $false
    try {
        & $doctor -ManifestPath $duplicatePath -SkipLive -Strict | Out-Null
    }
    catch {
        if ($_.Exception.Message -ne "Fleet manifest machine IDs must be unique.") { throw }
        $rejected = $true
    }
    if (-not $rejected) {
        throw "Fleet doctor accepted duplicate machine IDs."
    }

    $routeControls = @(
        @{ config = @{ server_urls = @(' ', 'http://first.invalid:8400', 'http://second.invalid:8400'); server_url = 'http://legacy.invalid:8400' }; expected = 'http://first.invalid:8400' },
        @{ config = @{ server_urls = @(@{ url = '' }, @{ url = 'http://object.invalid:8400' }); server_url = 'http://legacy.invalid:8400' }; expected = 'http://object.invalid:8400' },
        @{ config = @{ server_urls = @('', @{ url = ' ' }); server_url = 'http://legacy.invalid:8400' }; expected = 'http://legacy.invalid:8400' },
        @{ config = @{ server_url = 'http://legacy.invalid:8400' }; expected = 'http://legacy.invalid:8400' }
    )
    foreach ($control in $routeControls) {
        if ((Get-FleetConfigRoute ([pscustomobject]$control.config)) -cne $control.expected) {
            throw 'Ordered route priority or blank fallback failed'
        }
    }
    $windowsMachine = [pscustomobject]@{
        id = 'carbon-fixture'; connection = 'ssh-windows'; os = 'windows'; architecture = 'x86_64'; role = 'client'
        ssh_host = 'dtm-carbon-two.vpn.dtmventures.com'; ssh_user = 'david'; ssh_port = 22; ssh_host_key_alias = 'dtm-carbon-two'
        canonical_repo = 'C:/codedev/agent-hub'; cli_path = 'C:/Users/david/bin/agent-bus.exe'
        config_path = 'C:/Users/david/.config/agent-bus/config.json'; client_server_url = 'http://localhost:18480'
        auth_source = 'client-config'; required_active_services = @(); required_inactive_services = @()
    }
    $windowsManifestPath = Join-Path $fixtureRoot 'windows-manifest.json'
    $windowsManifest = @{ schema_version = 1; authority_machine = 'authority-a'; expected_protocol_version = '1.0';
        expected_build_revision = $fullRevision; machines = @(
            @{ id = 'authority-a'; connection = 'ssh-linux'; ssh_host = 'authority-a'; os = 'linux'; architecture = 'x86_64';
                role = 'authority'; canonical_repo = '/repo'; cli_path = '/bin/agent-bus'; config_path = '/config.json';
                auth_source = 'hub-env'; client_server_url = 'http://localhost:8400' }, $windowsMachine) }
    $windowsManifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $windowsManifestPath -Encoding utf8
    & $doctor -ManifestPath $windowsManifestPath -SkipLive -Strict | Out-Null
    $command = Get-FleetWindowsCommand $windowsMachine
    $remoteScript = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String(($command -split ' ')[-1]))
    $remoteTokens = $null; $remoteErrors = $null
    $null = [Management.Automation.Language.Parser]::ParseInput($remoteScript, [ref]$remoteTokens, [ref]$remoteErrors)
    if ($remoteErrors.Count -or $remoteScript.Contains('fixture-only') -or $command -notmatch '^powershell\.exe .* -EncodedCommand [A-Za-z0-9+/=]+$') {
        throw 'Remote Windows projection was not safe encoded source'
    }
    $windowsMachine.cli_path = $cliPath
    $windowsMachine.config_path = $configPath
    $windowsMachine.client_server_url = 'http://authority.invalid:8400'
    $fixtureCommand = Get-FleetWindowsCommand $windowsMachine
    $fixtureScript = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String(($fixtureCommand -split ' ')[-1]))
    $previousUrl = $env:AGENT_BUS_SERVER_URL
    try {
        $env:AGENT_BUS_SERVER_URL = 'http://caller-route.invalid:8400'
        $projectionText = & ([scriptblock]::Create($fixtureScript)) | Out-String
        $projection = $projectionText | ConvertFrom-Json
        if ($projection.config.server_url -cne 'http://authority.invalid:8400' -or
            $projection.config.auth_token_present -ne $true -or $projection.health.ok -ne $true -or
            $projectionText.Contains('fixture-only') -or $projectionText.Contains('token_file')) {
            throw 'Actual encoded Windows projection failed its redaction or route control'
        }
    } finally {
        if ($null -eq $previousUrl) { Remove-Item Env:AGENT_BUS_SERVER_URL -ErrorAction SilentlyContinue }
        else { $env:AGENT_BUS_SERVER_URL = $previousUrl }
    }
    foreach ($path in @('C:/bad;echo injected', 'C:/bad$(echo injected)', 'C:/../config.json', "C:/bad`npath")) {
        if (Test-FleetWindowsPath $path) { throw 'Unsafe Windows path accepted' }
    }
    foreach ($hostValue in @('-oProxyCommand=bad', 'host;echo-bad', "host`nother", 'user@host')) {
        $refused = $false
        try { Invoke-RemoteFleetCommand -HostName $hostValue -CommandText 'unused' | Out-Null }
        catch { if ($_.Exception.Message -notlike 'Unsafe SSH host*') { throw }; $refused = $true }
        if (-not $refused) { throw 'SSH host injection was accepted' }
    }
    $windowsMachine.ssh_host = 'host;echo-bad'
    $windowsManifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $windowsManifestPath -Encoding utf8
    $refused = $false
    try { & $doctor -ManifestPath $windowsManifestPath -SkipLive -Strict | Out-Null }
    catch { if ($_.Exception.Message -ne 'Unsafe SSH host in fleet manifest') { throw }; $refused = $true }
    if (-not $refused) { throw 'Manifest SSH host injection was accepted' }
    $windowsMachine.ssh_host = 'dtm-carbon-two.vpn.dtmventures.com'
    $windowsMachine.cli_path = 'C:/bad;injection.exe'
    $refused = $false
    try { Get-FleetWindowsCommand $windowsMachine | Out-Null }
    catch { if ($_.Exception.Message -ne 'Unsafe Windows fleet path') { throw }; $refused = $true }
    if (-not $refused) { throw 'Encoded payload accepted an unsafe Windows path' }

    Write-Output "Fleet doctor fixtures passed, including ordered routes and Windows SSH safety."
}
finally {
    $resolvedFixture = (Resolve-Path -LiteralPath $fixtureRoot).Path
    if ($resolvedFixture -ne [IO.Path]::GetFullPath($fixtureRoot) -or
        -not $resolvedFixture.StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()), [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refuse cleanup outside the fixture directory"
    }
    Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
}
