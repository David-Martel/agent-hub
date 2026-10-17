param([string]$WatchdogPath = (Join-Path $PSScriptRoot 'agent-bus-watchdog.ps1'))
$ErrorActionPreference = 'Stop'

# Import only the actual four function definitions, never execute host bootstrap,
# real CLI/config discovery, EventLog registration or the watchdog's main block.
$tokens = $null; $parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($WatchdogPath, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ('Watchdog parse failed: ' + $parseErrors[0].Message) }
foreach ($name in @('Get-WatchdogAssessment', 'Test-WatchdogNotificationDue', 'Write-Alert', 'Write-Ok')) {
    $matches = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true) | Where-Object { $_.Name -eq $name })
    if ($matches.Count -ne 1) { throw "Missing or ambiguous actual function: $name" }
    . ([scriptblock]::Create($matches[0].Extent.Text))
}

$script:passed = 0
function Assert-Equal($Actual, $Expected, [string]$Because) {
    if ($Actual -cne $Expected) { throw "$Because expected=[$Expected] actual=[$Actual]" }
}
function Invoke-Case([string]$Name, [scriptblock]$Body) {
    & $Body
    $script:passed++
    Write-Output "PASS $Name"
}
function New-Health {
    return [pscustomobject]@{ ok = $true; hub_identity = 'asuspro13'; build_version = 'reviewed-build'
        maintenance = [pscustomobject]@{ service_name = 'AgentHub' }
        backend = [pscustomobject]@{ mode = 'remote'; url = 'http://authority.invalid:8400'; authoritative = $true; tried = @() }
        network_locations = @('fixture') }
}
$candidates = @([pscustomobject]@{ role = 'authoritative'; hub = 'asuspro13'; url = 'http://authority.invalid:8400' },
    [pscustomobject]@{ role = 'cloud'; url = 'https://cloud.invalid' })

# Protects: role-aware authority checks. Detects: cloud falsely called an impostor,
# or unknown/non-authoritative destinations promoted to healthy. Needs: no hardware,
# config, credentials, network or services. Breadcrumb: agent-bus-watchdog.ps1
# Get-WatchdogAssessment. Explicit group: all assessment_* cases below.
$assessmentCases = @(
    @{ name = 'assessment_expected_authority_is_healthy'; state = 'OK'; exit = 0; change = {} },
    @{ name = 'assessment_configured_cloud_is_degraded'; state = 'DEGRADED_ONSITE_AUTHORITY_UNAVAILABLE'; exit = 2; change = { $h.hub_identity = 'cloud'; $h.backend.url = 'https://cloud.invalid'; $h.backend.authoritative = $false } },
    @{ name = 'assessment_unknown_cloud_endpoint_is_critical'; state = 'IDENTITY_MISMATCH'; exit = 1; change = { $h.hub_identity = 'cloud'; $h.backend.url = 'https://unconfigured.invalid'; $h.backend.authoritative = $false } },
    @{ name = 'assessment_cloud_claiming_authority_is_critical'; state = 'IDENTITY_MISMATCH'; exit = 1; change = { $h.hub_identity = 'cloud'; $h.backend.url = 'https://cloud.invalid' } },
    @{ name = 'assessment_string_false_does_not_qualify_cloud'; state = 'IDENTITY_MISMATCH'; exit = 1; change = { $h.hub_identity = 'cloud'; $h.backend.url = 'https://cloud.invalid'; $h.backend.authoritative = 'false' } },
    @{ name = 'assessment_missing_authority_bit_does_not_qualify_cloud'; state = 'IDENTITY_MISMATCH'; exit = 1; change = { $h.hub_identity = 'cloud'; $h.backend.url = 'https://cloud.invalid'; $h.backend.authoritative = $null } },
    @{ name = 'assessment_wrong_identity_is_critical'; state = 'IDENTITY_MISMATCH'; exit = 1; change = { $h.hub_identity = 'unexpected' } },
    @{ name = 'assessment_known_island_build_is_critical'; state = 'IMPOSTOR'; exit = 1; change = { $h.build_version = '0.5.0-a24e573a' } },
    @{ name = 'assessment_known_island_identity_is_critical'; state = 'IMPOSTOR'; exit = 1; change = { $h.hub_identity = 'DTM-P1GEN7' } },
    @{ name = 'assessment_known_island_service_is_critical'; state = 'IMPOSTOR'; exit = 1; change = { $h.maintenance.service_name = 'DTM-P1GEN7' } },
    @{ name = 'assessment_absent_health_is_unreachable'; state = 'UNREACHABLE'; exit = 1; change = { $h = $null } },
    @{ name = 'assessment_local_backend_is_not_fleet'; state = 'UNREACHABLE'; exit = 1; change = { $h.backend.mode = 'local' } },
    @{ name = 'assessment_unhealthy_backend_is_critical'; state = 'UNHEALTHY'; exit = 1; change = { $h.ok = $false } },
    @{ name = 'assessment_string_false_health_is_critical'; state = 'UNHEALTHY'; exit = 1; change = { $h.ok = 'false' } },
    @{ name = 'assessment_string_true_health_is_critical'; state = 'UNHEALTHY'; exit = 1; change = { $h.ok = 'true' } },
    @{ name = 'assessment_numeric_health_is_critical'; state = 'UNHEALTHY'; exit = 1; change = { $h.ok = 1 } },
    @{ name = 'assessment_null_health_is_critical'; state = 'UNHEALTHY'; exit = 1; change = { $h.ok = $null } },
    @{ name = 'assessment_absent_health_ok_is_critical'; state = 'UNHEALTHY'; exit = 1; change = { $h.PSObject.Properties.Remove('ok') } },
    @{ name = 'assessment_missing_named_authority_is_critical'; state = 'UNCONFIGURED_AUTHORITY'; exit = 1; change = { $expectedHub = '' } },
    @{ name = 'assessment_expected_identity_without_authority_is_critical'; state = 'NONAUTHORITATIVE'; exit = 1; change = { $h.backend.authoritative = $false } },
    @{ name = 'assessment_expected_identity_string_true_is_critical'; state = 'NONAUTHORITATIVE'; exit = 1; change = { $h.backend.authoritative = 'true' } },
    @{ name = 'assessment_duplicate_cloud_candidate_is_critical'; state = 'IDENTITY_MISMATCH'; exit = 1; change = { $h.hub_identity = 'cloud'; $h.backend.url = 'https://cloud.invalid'; $h.backend.authoritative = $false; $localCandidates = @($candidates) + @($candidates[1]) } },
    @{ name = 'assessment_noncloud_role_cannot_be_degraded'; state = 'IDENTITY_MISMATCH'; exit = 1; change = { $h.hub_identity = 'cloud'; $h.backend.url = 'http://authority.invalid:8400'; $h.backend.authoritative = $false } },
    @{ name = 'assessment_unknown_endpoint_cannot_claim_expected_identity'; state = 'UNCONFIGURED_ENDPOINT'; exit = 1; change = { $h.backend.url = 'http://unknown.invalid:8400' } },
    @{ name = 'assessment_missing_selected_url_is_critical'; state = 'UNCONFIGURED_ENDPOINT'; exit = 1; change = { $h.backend.url = $null } }
)
foreach ($case in $assessmentCases) {
    Invoke-Case $case.name {
        $h = New-Health; $localCandidates = @($candidates); $expectedHub = 'asuspro13'
        . $case.change
        $result = Get-WatchdogAssessment -Health $h -Candidates $localCandidates -ExpectedHub $expectedHub
        Assert-Equal $result.state $case.state 'Authority classification'
        Assert-Equal $result.exit_code $case.exit 'Watchdog exit code'
        if ($case.state -eq 'DEGRADED_ONSITE_AUTHORITY_UNAVAILABLE') {
            Assert-Equal $result.level 'Warning' 'Cloud fallback must remain a visible warning'
        }
    }
}

$now = [DateTimeOffset]::Parse('2026-01-01T12:00:00Z')
# Protects: bounded duplicate suppression without hiding new incidents. Detects:
# refresh timestamps extending silence forever, bad clocks or state hiding alerts.
# Needs: only fixture state/injected clock. Breadcrumb: Test-WatchdogNotificationDue.
# Explicit group: all notification_* cases below.
$notificationCases = @(
    @{ name = 'notification_first_incident_is_immediate'; due = $true; prior = $null },
    @{ name = 'notification_same_incident_inside_window_is_throttled'; due = $false; prior = @{ message = 'failure'; detail = 'endpoint-a'; last_notification_attempt_utc = '2026-01-01T11:59:00Z' } },
    @{ name = 'notification_exact_cooldown_boundary_is_due'; due = $true; prior = @{ message = 'failure'; detail = 'endpoint-a'; last_notification_attempt_utc = '2026-01-01T11:30:00Z' } },
    @{ name = 'notification_changed_class_is_immediate'; due = $true; prior = @{ message = 'other'; detail = 'endpoint-a'; last_notification_attempt_utc = '2026-01-01T11:59:00Z' } },
    @{ name = 'notification_changed_endpoint_is_immediate'; due = $true; prior = @{ message = 'failure'; detail = 'endpoint-b'; last_notification_attempt_utc = '2026-01-01T11:59:00Z' } },
    @{ name = 'notification_malformed_time_does_not_suppress'; due = $true; prior = @{ message = 'failure'; detail = 'endpoint-a'; last_notification_attempt_utc = 'invalid' } },
    @{ name = 'notification_future_time_does_not_suppress'; due = $true; prior = @{ message = 'failure'; detail = 'endpoint-a'; last_notification_attempt_utc = '2026-01-01T12:01:00Z' } },
    @{ name = 'notification_legacy_state_uses_original_attempt_time'; due = $false; prior = @{ message = 'failure'; detail = 'endpoint-a'; timestamp_utc = '2026-01-01T11:59:00Z' } }
)
foreach ($case in $notificationCases) {
    Invoke-Case $case.name {
        Assert-Equal (Test-WatchdogNotificationDue -Previous $case.prior -Message 'failure' -Detail 'endpoint-a' -Now $now) $case.due 'Notification decision'
    }
}

# Protects: real alert persistence and recovery lifecycle. Detects: modal storms,
# lost warning state, moving attempt time, stale flags or suppressed recurrence.
# Needs: owned temporary directory; msg/EventLog are stubbed before function calls.
# Breadcrumb: agent-bus-watchdog.ps1 Write-Alert and Write-Ok.
$flagDir = Join-Path ([IO.Path]::GetTempPath()) ('agent-bus-watchdog-fixtures-' + [guid]::NewGuid().ToString('N'))
$flagFile = Join-Path $flagDir 'alert.json'; $okFile = Join-Path $flagDir 'ok.json'; $logSource = 'fixture'
$script:notifications = 0; $script:eventIds = @()
function msg.exe { $script:notifications++ }
function Write-EventLog { param($LogName, $Source, $EntryType, $EventId, $Message, $ErrorAction) $script:eventIds += $EventId }
try {
    Invoke-Case 'lifecycle_repeat_recovery_and_recurrence_preserve_notifications' {
        Write-Alert 'DEGRADED_ONSITE_AUTHORITY_UNAVAILABLE' 'fixture-cloud' -Level Warning -Now $now | Out-Null
        Assert-Equal $script:notifications 1 'First incident must notify'
        Write-Alert 'DEGRADED_ONSITE_AUTHORITY_UNAVAILABLE' 'fixture-cloud' -Level Warning -Now $now.AddMinutes(10) | Out-Null
        Assert-Equal $script:notifications 1 'Identical incident must not repeat its popup'
        $state = Get-Content $flagFile -Raw | ConvertFrom-Json
        Assert-Equal ([DateTimeOffset]::Parse($state.last_notification_attempt_utc)) $now 'Repeat must not move notification clock'
        Assert-Equal (Test-Path $flagFile) $true 'Throttled incident must retain its alert flag'
        Write-Alert 'hub identity mismatch' 'fixture-unknown' -Now $now.AddMinutes(11) | Out-Null
        Assert-Equal $script:notifications 2 'Changed incident must notify immediately'
        Write-Ok 'fixture authority recovered'
        Assert-Equal (Test-Path $flagFile) $false 'Recovery must clear incident state'
        Assert-Equal (Test-Path $okFile) $true 'Recovery must persist a good receipt'
        Assert-Equal $script:eventIds[-1] 8401 'Recovery must emit a recovery event'
        Write-Alert 'hub identity mismatch' 'fixture-unknown' -Now $now.AddMinutes(12) | Out-Null
        Assert-Equal $script:notifications 3 'Post-recovery recurrence must notify immediately'
        Write-Alert 'hub identity mismatch' 'fixture-unknown' -Now $now.AddMinutes(42) | Out-Null
        Assert-Equal $script:notifications 4 'Persistent incident must remind at the cooldown boundary'
    }
} finally {
    if (Test-Path $flagDir) { Remove-Item -LiteralPath $flagDir -Recurse -Force }
}
Write-Output "RESULT passed=$script:passed failed=0 skipped=0 runtime=$($PSVersionTable.PSVersion)"
