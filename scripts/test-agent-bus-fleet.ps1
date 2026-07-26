param(
    [string]$ManifestPath = (Join-Path (Split-Path -Parent $PSScriptRoot) "config/fleet/agent-bus-fleet-v1.json"),
    [string]$ExpectedBuildRevision = "",
    [switch]$SkipLive,
    [switch]$Strict,
    [switch]$Json
)

$ErrorActionPreference = "Stop"
$results = [System.Collections.Generic.List[object]]::new()

function Add-FleetCheck {
    param(
        [Parameter(Mandatory = $true)][string]$Machine,
        [Parameter(Mandatory = $true)][string]$Check,
        [Parameter(Mandatory = $true)][ValidateSet("ok", "warn", "fail", "skipped")][string]$Status,
        [Parameter(Mandatory = $true)][string]$Detail
    )

    $results.Add([pscustomobject]@{
            machine = $Machine
            check   = $Check
            status  = $Status
            detail  = $Detail
        })
}

function Test-SafeFleetIdentifier {
    param([Parameter(Mandatory = $true)][string]$Value)

    return $Value -match '^[0-9A-Za-z._@-]+$'
}

function Invoke-RemoteFleetCommand {
    param(
        [Parameter(Mandatory = $true)][string]$HostName,
        [Parameter(Mandatory = $true)][string]$CommandText
    )

    if (-not (Test-SafeFleetIdentifier -Value $HostName)) {
        throw "Unsafe SSH host in fleet manifest: $HostName"
    }
    $output = & ssh $HostName $CommandText 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "SSH command failed on ${HostName}: $($output -join ' ')"
    }
    return ($output -join "`n").Trim()
}

function Test-BuildRevision {
    param(
        [Parameter(Mandatory = $true)][string]$Machine,
        [Parameter(Mandatory = $true)][string]$VersionText,
        [Parameter(Mandatory = $true)][string]$Revision
    )

    if ($VersionText -match "(?i)-g$([regex]::Escape($Revision))(?:[ )-]|$)") {
        Add-FleetCheck -Machine $Machine -Check "build-revision" -Status "ok" -Detail "Reports g$Revision"
    }
    else {
        Add-FleetCheck -Machine $Machine -Check "build-revision" -Status "fail" -Detail "Expected g$Revision; observed '$VersionText'"
    }
}

function Test-HealthDocument {
    param(
        [Parameter(Mandatory = $true)][string]$Machine,
        [Parameter(Mandatory = $true)]$Health,
        [Parameter(Mandatory = $true)][string]$ProtocolVersion
    )

    if ($Health.ok -eq $true -and $Health.storage_ready -eq $true) {
        Add-FleetCheck -Machine $Machine -Check "health" -Status "ok" -Detail "Redis and PostgreSQL ready"
    }
    else {
        Add-FleetCheck -Machine $Machine -Check "health" -Status "fail" -Detail "Health did not report ready storage"
    }
    if ([string]$Health.protocol_version -eq $ProtocolVersion) {
        Add-FleetCheck -Machine $Machine -Check "protocol" -Status "ok" -Detail "Protocol $ProtocolVersion"
    }
    else {
        Add-FleetCheck -Machine $Machine -Check "protocol" -Status "fail" -Detail "Expected $ProtocolVersion; observed '$($Health.protocol_version)'"
    }
    if ($Health.pg_dropped_writes -eq 0 -and $Health.pg_write_errors -eq 0) {
        Add-FleetCheck -Machine $Machine -Check "write-integrity" -Status "ok" -Detail "No dropped PostgreSQL writes or write errors"
    }
    else {
        Add-FleetCheck -Machine $Machine -Check "write-integrity" -Status "fail" -Detail "Dropped writes=$($Health.pg_dropped_writes), write errors=$($Health.pg_write_errors)"
    }
}

if (-not (Test-Path -LiteralPath $ManifestPath)) {
    throw "Fleet manifest not found: $ManifestPath"
}
$manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json -Depth 20
if ($manifest.schema_version -ne 1) {
    throw "Unsupported fleet manifest schema_version '$($manifest.schema_version)'."
}
if ([string]::IsNullOrWhiteSpace([string]$manifest.authority_machine)) {
    throw "Fleet manifest authority_machine is required."
}
$machines = @($manifest.machines)
if ($machines.Count -eq 0) {
    throw "Fleet manifest must contain at least one machine."
}
$machineIds = @($machines | ForEach-Object { [string]$_.id })
if (@($machineIds | Sort-Object -Unique).Count -ne $machineIds.Count) {
    throw "Fleet manifest machine IDs must be unique."
}
$authority = @($machines | Where-Object { $_.id -eq $manifest.authority_machine })
if ($authority.Count -ne 1 -or $authority[0].role -ne "authority") {
    throw "authority_machine must identify exactly one machine with role=authority."
}

$revision = if ([string]::IsNullOrWhiteSpace($ExpectedBuildRevision)) {
    [string]$manifest.expected_build_revision
}
else {
    $ExpectedBuildRevision
}
if ($revision -notmatch '^[0-9a-fA-F]{7,40}$') {
    throw "Expected build revision must be a 7-40 character Git hex revision."
}

foreach ($machine in $machines) {
    $machineId = [string]$machine.id
    if (-not (Test-SafeFleetIdentifier -Value $machineId)) {
        throw "Unsafe fleet machine ID: $machineId"
    }
    if ($machine.connection -notin @("local-windows", "ssh-linux")) {
        throw "Unsupported connection '$($machine.connection)' for $machineId."
    }
    if ($machine.architecture -notin @("x86_64", "aarch64")) {
        throw "Unsupported architecture '$($machine.architecture)' for $machineId."
    }
    if ([string]::IsNullOrWhiteSpace([string]$machine.canonical_repo)) {
        throw "canonical_repo is required for $machineId."
    }
    if ([string]$machine.client_server_url -notmatch '^http://[0-9A-Za-z._-]+:[0-9]+$') {
        throw "client_server_url must be a stable HTTP hostname and port for $machineId."
    }
    if ($machine.auth_source -notin @("client-config", "hub-env")) {
        throw "auth_source must be client-config or hub-env for $machineId."
    }
    if ($machine.auth_source -eq "hub-env" -and $machine.role -ne "authority") {
        throw "Only the authority machine may use auth_source=hub-env."
    }
    Add-FleetCheck -Machine $machineId -Check "manifest" -Status "ok" -Detail "$($machine.role) $($machine.os)/$($machine.architecture)"
}

if ($SkipLive) {
    foreach ($machine in $machines) {
        Add-FleetCheck -Machine $machine.id -Check "live" -Status "skipped" -Detail "Skipped by -SkipLive"
    }
}
else {
    foreach ($machine in $machines) {
        $machineId = [string]$machine.id
        try {
            if ($machine.connection -eq "local-windows") {
                $versionText = (& $machine.cli_path --version 2>&1 | Out-String).Trim()
                Test-BuildRevision -Machine $machineId -VersionText $versionText -Revision $revision

                $config = Get-Content -LiteralPath $machine.config_path -Raw | ConvertFrom-Json
                $effectiveServerUrl = [string]$config.server_url
                if ([string]::IsNullOrWhiteSpace($effectiveServerUrl) -and $machine.allow_default_server_url -eq $true) {
                    $effectiveServerUrl = "http://localhost:8400"
                }
                if ($effectiveServerUrl -eq [string]$machine.client_server_url) {
                    Add-FleetCheck -Machine $machineId -Check "route" -Status "ok" -Detail $effectiveServerUrl
                }
                else {
                    Add-FleetCheck -Machine $machineId -Check "route" -Status "fail" -Detail "Expected $($machine.client_server_url); observed '$($config.server_url)'"
                }
                if ([string]::IsNullOrWhiteSpace([string]$config.auth_token)) {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "fail" -Detail "Client config has no bearer token source"
                }
                else {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "ok" -Detail "Bearer token source present (redacted)"
                }

                $priorServerUrl = $env:AGENT_BUS_SERVER_URL
                try {
                    $env:AGENT_BUS_SERVER_URL = [string]$machine.client_server_url
                    $health = (& $machine.cli_path health --encoding json | Out-String) | ConvertFrom-Json
                }
                finally {
                    if ([string]::IsNullOrEmpty($priorServerUrl)) {
                        Remove-Item Env:AGENT_BUS_SERVER_URL -ErrorAction SilentlyContinue
                    }
                    else {
                        $env:AGENT_BUS_SERVER_URL = $priorServerUrl
                    }
                }
                Test-HealthDocument -Machine $machineId -Health $health -ProtocolVersion $manifest.expected_protocol_version

                foreach ($serviceName in @($machine.required_active_services)) {
                    $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
                    $status = if ($service) { [string]$service.Status } else { "not-found" }
                    if ($status -eq "Running") {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "ok" -Detail "active"
                    }
                    else {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "fail" -Detail $status
                    }
                }
                foreach ($serviceName in @($machine.required_inactive_services)) {
                    $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
                    $status = if ($service) { [string]$service.Status } else { "not-found" }
                    if ($status -ne "Running") {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "ok" -Detail $status
                    }
                    else {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "fail" -Detail "active"
                    }
                }
            }
            else {
                $hostName = [string]$machine.ssh_host
                $versionText = Invoke-RemoteFleetCommand -HostName $hostName -CommandText '"$HOME/.local/bin/agent-bus" --version'
                Test-BuildRevision -Machine $machineId -VersionText $versionText -Revision $revision

                $configSummaryText = Invoke-RemoteFleetCommand -HostName $hostName -CommandText 'jq -c ''{server_url,auth_token_present:((.auth_token|type)=="string" and (.auth_token|length)>0)}'' "$HOME/.config/agent-bus/config.json"'
                $configSummary = $configSummaryText | ConvertFrom-Json
                $effectiveServerUrl = [string]$configSummary.server_url
                if ([string]::IsNullOrWhiteSpace($effectiveServerUrl) -and $machine.allow_default_server_url -eq $true) {
                    $effectiveServerUrl = "http://localhost:8400"
                }
                if ($effectiveServerUrl -eq [string]$machine.client_server_url) {
                    Add-FleetCheck -Machine $machineId -Check "route" -Status "ok" -Detail $effectiveServerUrl
                }
                else {
                    Add-FleetCheck -Machine $machineId -Check "route" -Status "fail" -Detail "Expected $($machine.client_server_url); observed '$($configSummary.server_url)'"
                }
                $authSourcePresent = $configSummary.auth_token_present -eq $true
                if ($machine.auth_source -eq "hub-env") {
                    $hubEnvAuth = Invoke-RemoteFleetCommand -HostName $hostName -CommandText 'if grep -Eq ''^AGENT_BUS_AUTH_TOKEN=.+$'' "$HOME/.config/agent-bus/hub.env"; then echo true; else echo false; fi'
                    $authSourcePresent = $hubEnvAuth -eq "true"
                }
                if ($authSourcePresent) {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "ok" -Detail "Bearer token source present (redacted)"
                }
                else {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "fail" -Detail "Client config has no bearer token source"
                }

                if (-not [string]::IsNullOrWhiteSpace([string]$machine.required_config_mode)) {
                    $mode = Invoke-RemoteFleetCommand -HostName $hostName -CommandText 'stat -c %a "$HOME/.config/agent-bus/config.json"'
                    if ($mode -eq [string]$machine.required_config_mode) {
                        Add-FleetCheck -Machine $machineId -Check "config-mode" -Status "ok" -Detail $mode
                    }
                    else {
                        Add-FleetCheck -Machine $machineId -Check "config-mode" -Status "fail" -Detail "Expected $($machine.required_config_mode); observed '$mode'"
                    }
                }

                $healthText = Invoke-RemoteFleetCommand -HostName $hostName -CommandText '"$HOME/.local/bin/agent-bus" health --encoding json'
                $health = $healthText | ConvertFrom-Json
                Test-HealthDocument -Machine $machineId -Health $health -ProtocolVersion $manifest.expected_protocol_version

                foreach ($serviceName in @($machine.required_active_services)) {
                    if (-not (Test-SafeFleetIdentifier -Value $serviceName)) {
                        throw "Unsafe service name for ${machineId}: $serviceName"
                    }
                    $state = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "systemctl --user is-active $serviceName || true"
                    if ($state -eq "active") {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "ok" -Detail $state
                    }
                    else {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "fail" -Detail $state
                    }
                }
                foreach ($serviceName in @($machine.required_inactive_services)) {
                    if (-not (Test-SafeFleetIdentifier -Value $serviceName)) {
                        throw "Unsafe service name for ${machineId}: $serviceName"
                    }
                    $state = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "systemctl --user is-active $serviceName || true"
                    if ($state -ne "active") {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "ok" -Detail $state
                    }
                    else {
                        Add-FleetCheck -Machine $machineId -Check "service:$serviceName" -Status "fail" -Detail $state
                    }
                }
            }
        }
        catch {
            Add-FleetCheck -Machine $machineId -Check "live" -Status "fail" -Detail $_.Exception.Message
        }
    }
}

if ($Json) {
    $results | ConvertTo-Json -Depth 10
}
else {
    $results | Format-Table -AutoSize
}

$failures = @($results | Where-Object { $_.status -eq "fail" })
if ($Strict -and $failures.Count -gt 0) {
    throw "Fleet doctor found $($failures.Count) failure(s)."
}
