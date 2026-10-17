# Agent-bus hub identity watchdog for dtm-p1gen7.
# Verifies the CONFIGURED hub (read live from ~/.config/agent-bus/config.json, so it tracks
# whatever the current authoritative endpoint is) is reachable AND is the real fleet hub,
# not a same-port impostor (e.g. a local dev/island instance bound to the same port number --
# this exact failure mode silently misrouted codex/Claude Code for weeks on 2026-08-20..09-27).
# Loud on failure: Windows Application Event Log (source AgentBusWatchdog) + a flag file.

$ErrorActionPreference = 'Stop'
$logSource = 'AgentBusWatchdog'
$flagDir = Join-Path $env:USERPROFILE '.config\agent-bus'
$flagFile = Join-Path $flagDir 'watchdog-last-alert.json'
$okFile = Join-Path $flagDir 'watchdog-last-ok.json'

# Known-bad fingerprints: the p1gen7 local island build. If the hub we reach reports this,
# it is the impostor, not the fleet hub, even though it answered on the configured port.
$islandBuildMarker = 'a24e573a'
$islandIdentityMarker = 'DTM-P1GEN7'

if (-not [System.Diagnostics.EventLog]::SourceExists($logSource)) {
    try { New-EventLog -LogName Application -Source $logSource -ErrorAction Stop } catch { }
}

function Get-WatchdogAssessment {
    param($Health, [object[]]$Candidates, [string]$ExpectedHub,
        [string]$IslandBuild = 'a24e573a', [string]$IslandIdentity = 'DTM-P1GEN7')
    if (-not $Health -or $Health.backend.mode -ne 'remote') {
        $tried = if ($Health) { @($Health.backend.tried) -join ', ' } else { 'agent-bus health produced no output' }
        $locations = if ($Health) { @($Health.network_locations) -join ',' } else { '?' }
        return @{ state = 'UNREACHABLE'; message = 'hub unreachable'; detail = "network_locations=$locations tried=$tried"; level = 'Error'; exit_code = 1 }
    }
    $hubUrl = $Health.backend.url
    if ($Health.ok -isnot [bool] -or -not $Health.ok) {
        return @{ state = 'UNHEALTHY'; message = 'hub reports ok=false'; detail = "hub=$hubUrl build_version=$($Health.build_version)"; level = 'Error'; exit_code = 1 }
    }
    if ($Health.build_version -match [regex]::Escape($IslandBuild) -or
        $Health.maintenance.service_name -eq $IslandIdentity -or $Health.hub_identity -eq $IslandIdentity) {
        return @{ state = 'IMPOSTOR'; message = 'IMPOSTOR HUB DETECTED'; detail = "hub=$hubUrl is answering with the LOCAL ISLAND's fingerprint (build_version=$($Health.build_version)), not the fleet hub. A same-port local service is shadowing the real hub again."; level = 'Error'; exit_code = 1 }
    }
    if (-not $ExpectedHub) {
        return @{ state = 'UNCONFIGURED_AUTHORITY'; message = 'config.json has no named authoritative hub'; detail = 'Refusing to treat an unnamed or cloud-only configuration as the fleet authority.'; level = 'Error'; exit_code = 1 }
    }
    if ($Health.hub_identity -cne $ExpectedHub) {
        $matched = @($Candidates | Where-Object {
            $_ -and $_.role -ceq 'cloud' -and $_.url -is [string] -and $_.url -and
            $hubUrl -is [string] -and $hubUrl -and $_.url.TrimEnd('/') -ceq $hubUrl.TrimEnd('/')
        })
        if ($matched.Count -eq 1 -and $Health.backend.authoritative -is [bool] -and
            -not $Health.backend.authoritative -and $Health.hub_identity -ceq 'cloud') {
            return @{ state = 'DEGRADED_ONSITE_AUTHORITY_UNAVAILABLE'; message = 'DEGRADED_ONSITE_AUTHORITY_UNAVAILABLE'; detail = "expected=$ExpectedHub selected_role=cloud hub=$hubUrl; configured fallback is reachable; onsite authority is unavailable"; level = 'Warning'; exit_code = 2 }
        }
        return @{ state = 'IDENTITY_MISMATCH'; message = 'hub identity mismatch'; detail = "hub=$hubUrl reports hub_identity='$($Health.hub_identity)', expected '$ExpectedHub'."; level = 'Error'; exit_code = 1 }
    }
    if ($Health.backend.authoritative -isnot [bool] -or -not $Health.backend.authoritative) {
        return @{ state = 'NONAUTHORITATIVE'; message = 'onsite hub is not authoritative'; detail = "hub=$hubUrl identity=$ExpectedHub did not confirm authoritative=true"; level = 'Error'; exit_code = 1 }
    }
    $authority = @($Candidates | Where-Object {
        $_ -and $_.role -ceq 'authoritative' -and $_.hub -ceq $ExpectedHub -and
        $_.url -is [string] -and $_.url -and $hubUrl -is [string] -and
        $hubUrl -and $_.url.TrimEnd('/') -ceq $hubUrl.TrimEnd('/')
    })
    if ($authority.Count -ne 1) {
        return @{ state = 'UNCONFIGURED_ENDPOINT'; message = 'selected endpoint is not the configured authority'; detail = "hub=$hubUrl identity=$ExpectedHub has no unique matching authoritative candidate"; level = 'Error'; exit_code = 1 }
    }
    return @{ state = 'OK'; exit_code = 0 }
}

function Test-WatchdogNotificationDue {
    param($Previous, [string]$Message, [string]$Detail,
        [DateTimeOffset]$Now, [ValidateRange(1, 86400)][int]$CooldownSeconds = 1800)
    if (-not $Previous -or $Previous.message -cne $Message -or
        $Previous.detail -cne $Detail) { return $true }
    $last = $Previous.last_notification_attempt_utc
    if (-not $last) { $last = $Previous.timestamp_utc }
    $parsed = [DateTimeOffset]::MinValue
    if (-not [DateTimeOffset]::TryParse([string]$last, [ref]$parsed) -or
        $parsed -gt $Now) { return $true }
    return (($Now - $parsed).TotalSeconds -ge $CooldownSeconds)
}

function Write-Alert {
    param([string]$Message, [string]$Detail,
        [ValidateSet('Error', 'Warning')][string]$Level = 'Error',
        [DateTimeOffset]$Now = [DateTimeOffset]::UtcNow)
    $previous = $null
    if (Test-Path $flagFile) {
        try { $previous = Get-Content $flagFile -Raw | ConvertFrom-Json } catch { }
    }
    $notify = Test-WatchdogNotificationDue -Previous $previous -Message $Message -Detail $Detail -Now $Now
    $lastAttempt = if ($notify) { $Now.ToString('o') }
        elseif ($previous.last_notification_attempt_utc) { $previous.last_notification_attempt_utc }
        else { $previous.timestamp_utc }
    try { Write-EventLog -LogName Application -Source $logSource -EntryType $Level -EventId 8400 -Message "$Message`n$Detail" -ErrorAction SilentlyContinue } catch { }
    $payload = @{ timestamp_utc = $Now.ToString('o'); message = $Message; detail = $Detail
        last_notification_attempt_utc = $lastAttempt; notification_attempted = $notify } | ConvertTo-Json -Compress
    New-Item -ItemType Directory -Path $flagDir -Force -ErrorAction SilentlyContinue | Out-Null
    Set-Content -Path $flagFile -Value $payload -Encoding UTF8
    if ($notify) { try { msg.exe * "AGENT-BUS WATCHDOG ALERT: $Message" 2>$null } catch { } }
    Write-Output "ALERT: $Message`n$Detail"
}

function Write-Ok($detail) {
    $recovering = Test-Path $flagFile
    $payload = @{ timestamp_utc = (Get-Date).ToUniversalTime().ToString('o'); detail = $detail } | ConvertTo-Json -Compress
    New-Item -ItemType Directory -Path $flagDir -Force -ErrorAction SilentlyContinue | Out-Null
    Set-Content -Path $okFile -Value $payload -Encoding UTF8
    Remove-Item $flagFile -ErrorAction SilentlyContinue
    if ($recovering) {
        try { Write-EventLog -LogName Application -Source $logSource -EntryType Information -EventId 8401 -Message "AGENT-BUS WATCHDOG RECOVERED`n$detail" -ErrorAction SilentlyContinue } catch { }
    }
}


try {
    $cfgPath = Join-Path $env:USERPROFILE '.config\agent-bus\config.json'
    if (-not (Test-Path $cfgPath)) {
        Write-Alert "config.json missing" "expected at $cfgPath"
        exit 1
    }
    $cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
    # Since agent-hub #103 the client routes by network location over an ordered
    # `server_urls` list (fabric p2p, UMich ssh forward, cloud), so ask the client
    # itself which hub it reaches instead of probing one fixed URL.
    $routes = @($cfg.server_urls) + @($cfg.server_url) | Where-Object { $_ }
    if (-not $routes) {
        Write-Alert "config.json has no hub candidates" "p1gen7 should be an HTTP client of the fleet hub; a config with neither server_urls nor server_url falls back to a local backend silently."
        exit 1
    }
    $expectedHub = @($cfg.server_urls | Where-Object { $_.role -eq 'authoritative' -and $_.hub } | ForEach-Object { $_.hub }) | Select-Object -First 1
    $agentBus = Join-Path $env:USERPROFILE 'bin\agent-bus.exe'

    function Get-BusHealth {
        $env:AGENT_BUS_MACHINE_SAFE = 'true'
        $raw = & $agentBus health --encoding json 2>$null
        if (-not $raw) { return $null }
        return ($raw | Out-String | ConvertFrom-Json)
    }

    $body = Get-BusHealth
    # On the UMich network / VPN the hub is only reachable through the vigil1
    # ssh forward; start it (idempotent) once before alerting. Port 18480, not
    # 18400: the local AgentHub (NSSM) island service owns localhost:18400.
    if ((-not $body -or $body.backend.mode -ne 'remote') -and $body -and (@($body.network_locations) -contains 'umich')) {
        try {
            & pwsh -NoProfile -File 'C:\codedev\agent-bus\scripts\agent-bus-tunnel.ps1' -Jump 'vigil@141.212.75.201' -JumpPort 2222 -Target '10.60.4.1:8400' -LocalPort 18480 -TimeoutSeconds 30 | Out-Null
        } catch { }
        $body = Get-BusHealth
    }
    $assessment = Get-WatchdogAssessment -Health $body -Candidates @($cfg.server_urls) -ExpectedHub $expectedHub -IslandBuild $islandBuildMarker -IslandIdentity $islandIdentityMarker
    if ($assessment.state -ne 'OK') {
        Write-Alert -Message $assessment.message -Detail $assessment.detail -Level $assessment.level
        exit $assessment.exit_code
    }
    $hubUrl = $body.backend.url
    Write-Ok "hub=$hubUrl authoritative=$($body.backend.authoritative) hub_identity=$($body.hub_identity) network_locations=$(@($body.network_locations) -join ',') build_version=$($body.build_version) pg_message_count=$($body.pg_message_count)"
    exit 0
} catch {
    Write-Alert "watchdog script error" $_.Exception.Message
    exit 1
}
