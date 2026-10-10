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

function ConvertTo-PosixShellLiteral {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ($Path -notmatch '^/[0-9A-Za-z._/+-]+$') {
        throw "Unsafe absolute POSIX path in fleet manifest: $Path"
    }
    return "'$Path'"
}

function Invoke-RemoteFleetCommand {
    param(
        [Parameter(Mandatory = $true)][string]$HostName,
        [Parameter(Mandatory = $true)][string]$CommandText,
        [string]$UserName,
        [int]$Port = 22,
        [string]$HostKeyAlias
    )

    if ($HostName -cnotmatch '\A[0-9A-Za-z](?:[0-9A-Za-z.-]*[0-9A-Za-z])?\z') {
        throw "Unsafe SSH host in fleet manifest: $HostName"
    }
    if ($Port -lt 1 -or $Port -gt 65535 -or
        ($UserName -and $UserName -cnotmatch '\A[0-9A-Za-z_][0-9A-Za-z_.-]*\z') -or
        ($HostKeyAlias -and $HostKeyAlias -cnotmatch '\A[0-9A-Za-z][0-9A-Za-z.-]*\z')) {
        throw "Unsafe SSH identity or port in fleet manifest"
    }
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = (Get-Command ssh -CommandType Application -TotalCount 1).Source
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $arguments = @('-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', '-o', 'SendEnv=-*',
        '-o', 'UpdateHostKeys=no', '-o', 'ConnectTimeout=5', '-o', 'ConnectionAttempts=1',
        '-o', 'ControlMaster=no', '-o', 'ControlPath=none')
    if ($UserName) { $arguments += @('-l', $UserName) }
    if ($HostKeyAlias) { $arguments += @('-o', "HostKeyAlias=$HostKeyAlias") }
    # Keep saved Linux alias ports; the explicit port is for the Windows projection.
    if ($PSBoundParameters.ContainsKey('Port')) { $arguments += @('-p', [string]$Port) }
    foreach ($argument in ($arguments + @($HostName, $CommandText))) { $start.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $stdout = $null; $stderr = $null; $settled = $false; $deadlineHit = $false
    if (-not $process.Start()) { throw 'SSH client failed to start' }
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $deadlineHit = -not $process.WaitForExit([int][Math]::Max(0, 45000 - $clock.ElapsedMilliseconds))
    }
    finally {
        $settlement = [Diagnostics.Stopwatch]::StartNew()
        if (-not $process.HasExited) {
            $process.Kill($false) # Only this original SSH client; never a process tree.
            $null = $process.WaitForExit(2000)
        }
        if ($stdout -and $stderr) {
            $null = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@($stdout, $stderr)).Wait(
                [int][Math]::Max(0, 2000 - $settlement.ElapsedMilliseconds))
            $settled = $process.HasExited -and $stdout.IsCompletedSuccessfully -and $stderr.IsCompletedSuccessfully
        }
        if (-not $settled) {
            $script:RetainedFleetSshClient = @{ Process = $process; Stdout = $stdout; Stderr = $stderr }
        }
    }
    $deadlineHit = $deadlineHit -or $clock.ElapsedMilliseconds -ge 45000
    if (-not $settled) { throw 'SSH original client or capture remains unsettled' }
    try {
        if ($deadlineHit -or $process.ExitCode -ne 0) {
            throw "SSH read-only projection failed on $HostName (exit=$($process.ExitCode), deadline=$deadlineHit)"
        }
        return $stdout.GetAwaiter().GetResult().Trim()
    }
    finally { $process.Dispose() }
}

function Get-FleetConfigRoute {
    param([Parameter(Mandatory)]$Config)

    foreach ($candidate in @($Config.server_urls)) {
        $url = if ($candidate -is [string]) { $candidate } else { [string]$candidate.url }
        if (-not [string]::IsNullOrWhiteSpace($url)) { return $url.Trim() }
    }
    return ([string]$Config.server_url).Trim()
}

function Test-FleetConfigRoute {
    param([Parameter(Mandatory)]$Machine, [Parameter(Mandatory)]$Config)

    $route = Get-FleetConfigRoute $Config
    $defaultAuthority = $Machine.role -eq 'authority' -and $Machine.allow_default_server_url -eq $true -and
        -not $route -and $Machine.client_server_url -ceq 'http://localhost:8400'
    if ($route -ceq $Machine.client_server_url -or $defaultAuthority) {
        Add-FleetCheck -Machine $Machine.id -Check 'config-route' -Status 'ok' -Detail 'Ordered configuration route matches'
    } else {
        Add-FleetCheck -Machine $Machine.id -Check 'config-route' -Status 'fail' -Detail 'Ordered configuration route differs'
    }
}

function Test-FleetWindowsPath {
    param([Parameter(Mandatory)][string]$Path)

    return $Path -cmatch '\A[A-Za-z]:[\\/][0-9A-Za-z ._\\/-]+\z' -and
        $Path -notmatch '(^|[\\/])\.\.([\\/]|$)'
}

function Get-FleetWindowsCommand {
    param([Parameter(Mandatory)]$Machine)

    foreach ($path in @($Machine.cli_path, $Machine.config_path, $Machine.canonical_repo)) {
        if (-not (Test-FleetWindowsPath -Path ([string]$path))) { throw 'Unsafe Windows fleet path' }
    }
    $payload = @{ cli = [string]$Machine.cli_path; config = [string]$Machine.config_path;
        route = [string]$Machine.client_server_url;
        services = @(@($Machine.required_active_services) + @($Machine.required_inactive_services) | Where-Object { $null -ne $_ }) }
    $encodedPayload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(($payload | ConvertTo-Json -Compress)))
    $routeFunction = 'function Get-FleetConfigRoute {' + (Get-Command Get-FleetConfigRoute).ScriptBlock.ToString() + '}'
    $authFunction = 'function Test-FleetConfigAuthSource {' + (Get-Command Test-FleetConfigAuthSource).ScriptBlock.ToString() + '}'
    $remote = @'
$ErrorActionPreference = 'Stop'
try {
    $inputData = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__PAYLOAD__')) | ConvertFrom-Json
    __ROUTE_FUNCTION__
    __AUTH_FUNCTION__
    $config = Get-Content -LiteralPath $inputData.config -Raw | ConvertFrom-Json
    $version = (& $inputData.cli --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'CLI version failed' }
    $health = (& $inputData.cli health --encoding json | Out-String) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'CLI health failed' }
    $serviceStates = @{}
    foreach ($name in $inputData.services) {
        $service = Get-Service -Name $name -ErrorAction SilentlyContinue
        $serviceStates[$name] = if ($service) { [string]$service.Status } else { 'not-found' }
    }
    @{ version = $version; config = @{ server_url = (Get-FleetConfigRoute $config);
            auth_token_present = [bool](Test-FleetConfigAuthSource $config $inputData.route) };
        health = @{ ok = $health.ok; storage_ready = $health.storage_ready; protocol_version = $health.protocol_version;
            build_version = $health.build_version; hub_identity = $health.hub_identity;
            pg_dropped_writes = $health.pg_dropped_writes; pg_write_errors = $health.pg_write_errors;
            backend = @{ mode = $health.backend.mode; url = $health.backend.url;
                authoritative = $health.backend.authoritative; hub_build = $health.backend.hub_build } };
        services = $serviceStates } | ConvertTo-Json -Depth 8 -Compress
} catch {
    [Console]::Error.WriteLine('Fleet Windows projection failed: ' + $_.Exception.GetType().FullName)
    exit 1
}
'@
    $remote = $remote.Replace('__PAYLOAD__', $encodedPayload).Replace('__ROUTE_FUNCTION__', $routeFunction).Replace('__AUTH_FUNCTION__', $authFunction)
    return 'powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ' +
        [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($remote))
}

function Test-BuildRevisionMatch {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$VersionText,
        [Parameter(Mandatory = $true)][string]$Revision
    )

    if ($Revision -notmatch '^[0-9a-fA-F]{7,40}$' -or $VersionText -match '(?i)dirty') {
        return $false
    }
    # Parse the entire provenance token before comparing. A longer hexadecimal
    # token must never pass merely because it starts with the expected revision.
    if ($VersionText -notmatch '^(?:agent-bus(?:-http|-mcp)?\s+)?\d+\.\d+\.\d+ \((?<source>[^\s()]+)(?: \d{4}-\d{2}-\d{2})?\)$') {
        return $false
    }
    $source = $Matches.source
    if ($source -match '^[0-9a-fA-F]{7,40}$') {
        return $source -ieq $Revision
    }
    if ($source -match '^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?-\d+-g(?<revision>[0-9a-fA-F]{7,40})$') {
        return $Matches.revision -ieq $Revision
    }
    return $false
}

function Test-BuildRevision {
    param(
        [Parameter(Mandatory = $true)][string]$Machine,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$VersionText,
        [Parameter(Mandatory = $true)][string]$Revision,
        [string]$CheckName = "build-revision"
    )

    if (Test-BuildRevisionMatch -VersionText $VersionText -Revision $Revision) {
        Add-FleetCheck -Machine $Machine -Check $CheckName -Status "ok" -Detail "Reports exact clean revision $Revision"
    }
    else {
        Add-FleetCheck -Machine $Machine -Check $CheckName -Status "fail" -Detail "Expected exact clean revision $Revision; observed '$VersionText'"
    }
}

function Test-HealthDocument {
    param(
        [Parameter(Mandatory = $true)][string]$Machine,
        [Parameter(Mandatory = $true)]$Health,
        [Parameter(Mandatory = $true)][string]$ProtocolVersion,
        [Parameter(Mandatory = $true)][string]$Revision
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
    $serviceBuild = if ($Health.backend.mode -eq "remote") {
        [string]$Health.backend.hub_build
    } else {
        [string]$Health.build_version
    }
    Test-BuildRevision `
        -Machine $Machine `
        -VersionText $serviceBuild `
        -Revision $Revision `
        -CheckName "service-build-revision"
    if ($Health.pg_dropped_writes -eq 0 -and $Health.pg_write_errors -eq 0) {
        Add-FleetCheck -Machine $Machine -Check "write-integrity" -Status "ok" -Detail "No dropped PostgreSQL writes or write errors"
    }
    else {
        Add-FleetCheck -Machine $Machine -Check "write-integrity" -Status "fail" -Detail "Dropped writes=$($Health.pg_dropped_writes), write errors=$($Health.pg_write_errors)"
    }
}

function Test-FleetHealthRoute {
    param(
        [Parameter(Mandatory = $true)]$Machine,
        [Parameter(Mandatory = $true)]$Health,
        [Parameter(Mandatory = $true)][string]$AuthorityMachine
    )

    $mode = [string]$Health.backend.mode
    $url = [string]$Health.backend.url
    $expectedUrl = [string]$Machine.client_server_url
    $localAuthority = $Machine.role -eq "authority" -and $mode -eq "local" -and
        $Machine.allow_default_server_url -eq $true -and $expectedUrl -eq "http://localhost:8400"
    if (($mode -eq "remote" -and $url -ceq $expectedUrl) -or $localAuthority) {
        $detail = if ($localAuthority) { "Authority uses its local backend; HTTP listener still needs a separate probe" } else { $url }
        Add-FleetCheck -Machine $Machine.id -Check "route" -Status "ok" -Detail $detail
    } else {
        Add-FleetCheck -Machine $Machine.id -Check "route" -Status "fail" -Detail "Expected $expectedUrl; selected mode='$mode', url='$url'"
    }
    if (($mode -eq "remote" -and $Health.backend.authoritative -eq $true) -or $localAuthority) {
        Add-FleetCheck -Machine $Machine.id -Check "authority" -Status "ok" -Detail "Uses the claims authority"
    } else {
        Add-FleetCheck -Machine $Machine.id -Check "authority" -Status "fail" -Detail "Health did not establish an authoritative backend"
    }
    if ([string]$Health.hub_identity -ceq $AuthorityMachine) {
        Add-FleetCheck -Machine $Machine.id -Check "hub-identity" -Status "ok" -Detail $AuthorityMachine
    } else {
        Add-FleetCheck -Machine $Machine.id -Check "hub-identity" -Status "fail" -Detail "Expected $AuthorityMachine; reported '$($Health.hub_identity)'"
    }
}

function Test-FleetConfigAuthSource {
    param(
        [Parameter(Mandatory = $true)]$Config,
        [Parameter(Mandatory = $true)][string]$RouteUrl,
        [scriptblock]$TokenFilePresent = {
            param($Path)
            if ($Path.StartsWith('~/')) { $Path = Join-Path ([Environment]::GetFolderPath('UserProfile')) $Path.Substring(2) }
            return (Test-Path -LiteralPath $Path -PathType Leaf) -and (Get-Item -LiteralPath $Path).Length -gt 0
        }
    )

    $candidate = @($Config.server_urls | Where-Object { $_ -isnot [string] -and $_.url -ceq $RouteUrl })
    if ($candidate.Count -gt 1) { return $false }
    if ($candidate.Count -eq 1) {
        # Explicit sources replace the global credential, including an absent file.
        if (-not [string]::IsNullOrWhiteSpace([string]$candidate[0].token_file)) {
            return [bool](& $TokenFilePresent ([string]$candidate[0].token_file))
        }
        if (-not [string]::IsNullOrWhiteSpace([string]$candidate[0].token_env)) {
            # Environment-source qualification requires a separate host-specific
            # check; never substitute the global token for an explicit source.
            return $false
        }
    }
    if ($Config.PSObject.Properties['auth_token_present']) {
        return $Config.auth_token_present -eq $true
    }
    return -not [string]::IsNullOrWhiteSpace([string]$Config.auth_token)
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
    if ($machine.connection -notin @("local-windows", "ssh-linux", "ssh-windows")) {
        throw "Unsupported connection '$($machine.connection)' for $machineId."
    }
    if ($machine.architecture -notin @("x86_64", "aarch64")) {
        throw "Unsupported architecture '$($machine.architecture)' for $machineId."
    }
    if ([string]::IsNullOrWhiteSpace([string]$machine.canonical_repo)) {
        throw "canonical_repo is required for $machineId."
    }
    if ([string]::IsNullOrWhiteSpace([string]$machine.cli_path)) {
        throw "cli_path is required for $machineId."
    }
    if ([string]::IsNullOrWhiteSpace([string]$machine.config_path)) {
        throw "config_path is required for $machineId."
    }
    if ($machine.connection -eq "ssh-linux") {
        ConvertTo-PosixShellLiteral -Path ([string]$machine.cli_path) | Out-Null
        ConvertTo-PosixShellLiteral -Path ([string]$machine.config_path) | Out-Null
    }
    if ($machine.connection -in @('ssh-linux', 'ssh-windows') -and
        [string]$machine.ssh_host -cnotmatch '\A[0-9A-Za-z](?:[0-9A-Za-z.-]*[0-9A-Za-z])?\z') {
        throw 'Unsafe SSH host in fleet manifest'
    }
    if ($machine.connection -eq 'ssh-windows') {
        if ($machine.os -cne 'windows' -or [string]$machine.ssh_user -cnotmatch '\A[0-9A-Za-z_][0-9A-Za-z_.-]*\z' -or
            [string]$machine.ssh_host_key_alias -cnotmatch '\A[0-9A-Za-z][0-9A-Za-z.-]*\z' -or
            ($machine.ssh_port -isnot [long] -and $machine.ssh_port -isnot [int]) -or
            $machine.ssh_port -lt 1 -or $machine.ssh_port -gt 65535) { throw 'Invalid Windows SSH manifest identity' }
        Get-FleetWindowsCommand $machine | Out-Null
    }
    foreach ($serviceName in @(@($machine.required_active_services) + @($machine.required_inactive_services) | Where-Object { $null -ne $_ })) {
        if (-not (Test-SafeFleetIdentifier -Value ([string]$serviceName))) { throw 'Unsafe fleet service name' }
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
                if ($LASTEXITCODE -ne 0) { throw "CLI version command failed ($LASTEXITCODE)" }
                Test-BuildRevision -Machine $machineId -VersionText $versionText -Revision $revision

                $config = Get-Content -LiteralPath $machine.config_path -Raw | ConvertFrom-Json
                Test-FleetConfigRoute -Machine $machine -Config $config
                # Observe the host's real configuration and inherited overrides.
                # Forcing the manifest URL would conceal actual routing drift.
                $healthText = (& $machine.cli_path health --encoding json | Out-String)
                if ($LASTEXITCODE -ne 0) { throw "CLI health command failed ($LASTEXITCODE)" }
                $health = $healthText | ConvertFrom-Json
                Test-FleetHealthRoute -Machine $machine -Health $health -AuthorityMachine $manifest.authority_machine
                if (-not (Test-FleetConfigAuthSource -Config $config -RouteUrl ([string]$machine.client_server_url))) {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "fail" -Detail "Client config has no bearer token source"
                }
                else {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "ok" -Detail "Bearer token source present (redacted)"
                }

                Test-HealthDocument -Machine $machineId -Health $health -ProtocolVersion $manifest.expected_protocol_version -Revision $revision

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
            elseif ($machine.connection -eq 'ssh-windows') {
                $command = Get-FleetWindowsCommand $machine
                $projection = Invoke-RemoteFleetCommand -HostName $machine.ssh_host -UserName $machine.ssh_user `
                    -Port $machine.ssh_port -HostKeyAlias $machine.ssh_host_key_alias -CommandText $command | ConvertFrom-Json
                Test-BuildRevision -Machine $machineId -VersionText $projection.version -Revision $revision
                Test-FleetConfigRoute -Machine $machine -Config $projection.config
                $authStatus = if ($projection.config.auth_token_present -eq $true) { 'ok' } else { 'fail' }
                Add-FleetCheck -Machine $machineId -Check 'auth-source' -Status $authStatus -Detail 'Projected bearer source availability (redacted)'
                Test-FleetHealthRoute -Machine $machine -Health $projection.health -AuthorityMachine $manifest.authority_machine
                Test-HealthDocument -Machine $machineId -Health $projection.health -ProtocolVersion $manifest.expected_protocol_version -Revision $revision
                foreach ($name in @($machine.required_active_services)) {
                    $state = [string]$projection.services.$name
                    $status = if ($state -eq 'Running') { 'ok' } else { 'fail' }
                    Add-FleetCheck -Machine $machineId -Check "service:$name" -Status $status -Detail $state
                }
                foreach ($name in @($machine.required_inactive_services)) {
                    $state = [string]$projection.services.$name
                    $status = if ($state -ne 'Running') { 'ok' } else { 'fail' }
                    Add-FleetCheck -Machine $machineId -Check "service:$name" -Status $status -Detail $state
                }
            }
            else {
                $hostName = [string]$machine.ssh_host
                $cliShell = ConvertTo-PosixShellLiteral -Path ([string]$machine.cli_path)
                $configShell = ConvertTo-PosixShellLiteral -Path ([string]$machine.config_path)
                $hubEnvPath = ([string]$machine.config_path) -replace '/[^/]+$', '/hub.env'
                $hubEnvShell = ConvertTo-PosixShellLiteral -Path $hubEnvPath

                $versionText = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "$cliShell --version"
                Test-BuildRevision -Machine $machineId -VersionText $versionText -Revision $revision

                $configSummaryText = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "jq -c '{server_url:(([.server_urls[]? | if type==`"string`" then . else .url end | select(type==`"string`" and test(`"\\S`"))][0]) // .server_url // `"`"),auth_token_present:((.auth_token|type)==`"string`" and (.auth_token|length)>0),server_urls:[.server_urls[]? | if type==`"string`" then {url:.} else {url,role,hub,token_file,token_env} end]}' $configShell"
                $configSummary = $configSummaryText | ConvertFrom-Json
                Test-FleetConfigRoute -Machine $machine -Config $configSummary
                $tokenFilePresent = {
                    param($Path)
                    if ($Path -match '^~/[0-9A-Za-z._/+-]+$') {
                        $tokenShell = '"$HOME"/' + "'$($Path.Substring(2))'"
                    } else {
                        $tokenShell = ConvertTo-PosixShellLiteral -Path $Path
                    }
                    $present = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "if test -f $tokenShell && test -s $tokenShell; then echo true; else echo false; fi"
                    return $present -eq "true"
                }
                $authSourcePresent = Test-FleetConfigAuthSource -Config $configSummary -RouteUrl ([string]$machine.client_server_url) -TokenFilePresent $tokenFilePresent
                if ($machine.auth_source -eq "hub-env") {
                    $hubEnvAuth = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "if grep -Eq '^AGENT_BUS_AUTH_TOKEN=.+$' $hubEnvShell; then echo true; else echo false; fi"
                    $authSourcePresent = $hubEnvAuth -eq "true"
                }
                if ($authSourcePresent) {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "ok" -Detail "Bearer token source present (redacted)"
                }
                else {
                    Add-FleetCheck -Machine $machineId -Check "auth-source" -Status "fail" -Detail "Client config has no bearer token source"
                }

                if (-not [string]::IsNullOrWhiteSpace([string]$machine.required_config_mode)) {
                    $mode = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "stat -c %a $configShell"
                    if ($mode -eq [string]$machine.required_config_mode) {
                        Add-FleetCheck -Machine $machineId -Check "config-mode" -Status "ok" -Detail $mode
                    }
                    else {
                        Add-FleetCheck -Machine $machineId -Check "config-mode" -Status "fail" -Detail "Expected $($machine.required_config_mode); observed '$mode'"
                    }
                }

                $healthText = Invoke-RemoteFleetCommand -HostName $hostName -CommandText "$cliShell health --encoding json"
                $health = $healthText | ConvertFrom-Json
                Test-FleetHealthRoute -Machine $machine -Health $health -AuthorityMachine $manifest.authority_machine
                Test-HealthDocument -Machine $machineId -Health $health -ProtocolVersion $manifest.expected_protocol_version -Revision $revision

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
