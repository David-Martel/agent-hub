<#
.SYNOPSIS
Starts an existing interactive runner after its Linux Docker prerequisite is ready.
.DESCRIPTION
Use the existing owner's logon task, with IgnoreNew and restart-on-failure settings.
Does not register runners, switch engines, restart Docker or stop other processes.
The task remains interactive: unattended operation before user logon is not provided.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$RunnerDirectory = 'C:/actions-runner/agent-hub',
    [string]$DockerExecutable = 'C:/Program Files/Docker/Docker/resources/bin/docker.exe',
    [string]$ExpectedIdentity,
    [ValidateRange(10, 300)][int]$StartupTimeoutSeconds = 90
)

function Invoke-RunnerProbe {
    [CmdletBinding()]
    param([string]$Executable, [string[]]$Arguments, [int]$TimeoutSeconds = 15)
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($connectionOverride in @('DOCKER_HOST', 'DOCKER_CONTEXT', 'DOCKER_TLS_VERIFY', 'DOCKER_CERT_PATH')) {
        [void]$info.Environment.Remove($connectionOverride)
    }
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        [void]$process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $finished = $process.WaitForExit($TimeoutSeconds * 1000)
        if (-not $finished) {
            # Only this finite CLI client is owned here; never kill its descendants.
            $process.Kill()
            [void]$process.WaitForExit(1000)
        }
        $outputReady = $stdout.Wait(500)
        $errorReady = $stderr.Wait(500)
        [pscustomobject]@{
            Code = if ($finished) { $process.ExitCode } else { 124 }
            Output = if ($outputReady) { $stdout.Result.Trim() } else { '' }
            Error = if ($errorReady) { $stderr.Result.Trim() } else { 'CLI pipe did not settle' }
            ElapsedMilliseconds = $watch.ElapsedMilliseconds
            ClientPid = $process.Id
            TimedOut = -not $finished
        }
    } finally { $process.Dispose() }
}

function Get-RunnerHostProcess {
    $processes = @(Get-CimInstance Win32_Process)
    $services = @(Get-CimInstance Win32_Service | Where-Object {
        $_.Name -like 'actions.runner.*' -and $_.State -eq 'Running' -and $_.ProcessId -gt 0
    })
    foreach ($process in $processes) {
        $serviceListenerPath = $null
        if ($process.Name -eq 'Runner.Listener.exe' -and -not $process.ExecutablePath) {
            $parents = @($services | Where-Object { $_.ProcessId -eq $process.ParentProcessId })
            if ($parents.Count -eq 1 -and $parents[0].PathName -match
                '^\s*"?(?<binary>[A-Za-z]:[\\/].*?[\\/]bin[\\/]RunnerService\.exe)"?(?:\s|$)') {
                $serviceListenerPath = Join-Path ([IO.Path]::GetDirectoryName($Matches.binary)) 'Runner.Listener.exe'
            }
        }
        [pscustomobject]@{
            Name = $process.Name; ProcessId = $process.ProcessId
            ExecutablePath = $process.ExecutablePath
            ServiceListenerPath = $serviceListenerPath
        }
    }
}

function Start-RunnerDesktopApplication {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$Executable, [int]$TimeoutSeconds = 15)
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
        throw 'Supported Docker Desktop application is absent'
    }
    if ($PSCmdlet.ShouldProcess($Executable, 'Start the installed Docker Desktop application')) {
        # Bound the launch operation itself in an owned finite helper. Killing
        # that helper on deadline does not kill an application already launched.
        $launch = @'
$ErrorActionPreference = 'Stop'
$info = [Diagnostics.ProcessStartInfo]::new()
$info.FileName = '__APPLICATION__'
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
$process = [Diagnostics.Process]::Start($info)
if (-not $process) { throw 'Application launch returned no process' }
[Console]::WriteLine($process.Id)
$process.Dispose()
'@.Replace('__APPLICATION__', $Executable.Replace("'", "''"))
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($launch))
        $result = Invoke-RunnerProbe -Executable (Get-Process -Id $PID).Path -Arguments @(
            '-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded
        ) -TimeoutSeconds $TimeoutSeconds
        if ($result.Code -ne 0 -or $result.Output -notmatch '^\d+$') {
            throw 'Supported Docker Desktop application launch failed or exceeded its deadline'
        }
        return [int]$result.Output
    }
}

function Assert-RunnerListenerAbsent {
    param([object[]]$Processes, [string]$Listener)
    foreach ($process in $Processes | Where-Object Name -eq 'Runner.Listener.exe') {
        $path = if ($process.ExecutablePath) { $process.ExecutablePath } else { $process.ServiceListenerPath }
        if (-not $path) { throw 'An existing listener identity is unreadable; preserve it' }
        if ($path -ieq $Listener) { throw 'This registered runner already has a listener; preserve its lifecycle' }
    }
}

function Invoke-RegisteredRunnerListener {
    param([string]$Directory, [string]$Listener)
    Push-Location -LiteralPath $Directory
    try {
        & $Listener run --startuptype manual | Out-Host
        return $LASTEXITCODE
    } finally { Pop-Location }
}

function Write-RunnerReadiness {
    param([string]$Directory, [hashtable]$Receipt)
    $destination = Join-Path $Directory 'startup-readiness.json'
    $temporary = Join-Path $Directory ('.startup-readiness.' + $PID + '.tmp')
    [IO.File]::WriteAllText($temporary, ($Receipt | ConvertTo-Json -Depth 5),
        [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporary -Destination $destination -Force
}

function Start-RegisteredNativeRunner {
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [string]$Directory, [string]$Docker, [string]$Identity,
        [int]$TimeoutSeconds = 90
    )
    $actualIdentity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    if (-not $Identity -or $actualIdentity -ine $Identity) {
        throw 'Runner owner identity does not match the task configuration'
    }
    $listener = Join-Path $Directory 'bin/Runner.Listener.exe'
    foreach ($required in @($listener, $Docker, (Join-Path $Directory '.runner'))) {
        if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
            throw "Required runner prerequisite is absent: $required"
        }
    }
    $registration = [IO.File]::ReadAllText((Join-Path $Directory '.runner')) |
        ConvertFrom-Json -ErrorAction Stop
    if ($registration.gitHubUrl -ne 'https://github.com/David-Martel/agent-hub') {
        throw 'Existing runner registration belongs to a different repository'
    }
    $processes = @(Get-RunnerHostProcess)
    Assert-RunnerListenerAbsent -Processes $processes -Listener $listener
    if (-not $PSCmdlet.ShouldProcess($Directory, 'Start the existing runner after Linux Docker readiness')) {
        return 0
    }
    $receipt = @{
        Hostname = $env:COMPUTERNAME; Identity = $actualIdentity
        SessionId = (Get-Process -Id $PID).SessionId
        AtUtc = [DateTime]::UtcNow.ToString('o'); SupervisorPid = $PID
        StartedDesktop = $false; RunnerId = $registration.agentId
        SourceSha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash
    }
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $localInfoArguments = @('--host', 'npipe:////./pipe/dockerDesktopLinuxEngine', 'info', '--format', '{{.OSType}}')
    $probe = Invoke-RunnerProbe -Executable $Docker -Arguments $localInfoArguments `
        -TimeoutSeconds ([Math]::Min(15, $TimeoutSeconds))
    if ($probe.Code -eq 0 -and $probe.Output -ne 'linux') {
        throw 'Docker is running a different engine; engine switching is not authorized here'
    }
    if ($probe.Code -ne 0) {
        # Re-read immediately before launch; do not duplicate an application that
        # appeared during the finite API probe.
        $desktop = @(Get-RunnerHostProcess | Where-Object {
            $_.Name -match '^(Docker Desktop|com\.docker\.backend)(\.exe)?$'
        })
        if (-not $desktop.Count) {
            $remaining = [int][Math]::Floor(($deadline - [DateTime]::UtcNow).TotalSeconds)
            if ($remaining -le 0) { throw 'Docker startup deadline exceeded' }
            $desktopExecutable = Join-Path (Split-Path (Split-Path (Split-Path $Docker))) 'Docker Desktop.exe'
            $receipt.DesktopLauncherPid = Start-RunnerDesktopApplication -Executable $desktopExecutable `
                -TimeoutSeconds ([Math]::Min(15, $remaining))
            $receipt.StartedDesktop = $true
        }
        do {
            $remaining = [int][Math]::Ceiling(($deadline - [DateTime]::UtcNow).TotalSeconds)
            if ($remaining -le 0) { throw 'Linux Docker readiness deadline exceeded' }
            $probe = Invoke-RunnerProbe -Executable $Docker -Arguments $localInfoArguments `
                -TimeoutSeconds ([Math]::Min(15, $remaining))
            if ($probe.Code -eq 0) {
                if ($probe.Output -ne 'linux') { throw 'Unexpected Docker engine; preserve it' }
                break
            }
            Start-Sleep -Seconds 1
        } while ($true)
    }
    Assert-RunnerListenerAbsent -Processes @(Get-RunnerHostProcess) -Listener $listener
    $receipt.DockerOSType = $probe.Output
    $receipt.DockerInfoMilliseconds = $probe.ElapsedMilliseconds
    $receipt.ReadyAtUtc = [DateTime]::UtcNow.ToString('o')
    Write-RunnerReadiness -Directory $Directory -Receipt $receipt
    # Preserve the listener and startup type selected by the existing task.
    return (Invoke-RegisteredRunnerListener -Directory $Directory -Listener $listener)
}

if ($MyInvocation.InvocationName -ne '.') {
    $ErrorActionPreference = 'Stop'
    try {
        exit (Start-RegisteredNativeRunner -Directory $RunnerDirectory -Docker $DockerExecutable `
            -Identity $ExpectedIdentity -TimeoutSeconds $StartupTimeoutSeconds)
    } catch {
        # A hidden scheduled task still needs an inspectable failure receipt.
        $failure = $_
        if ($ExpectedIdentity -and
            [Security.Principal.WindowsIdentity]::GetCurrent().Name -ieq $ExpectedIdentity -and
            (Test-Path -LiteralPath (Join-Path $RunnerDirectory '.runner') -PathType Leaf)) {
            try {
                $failedRegistration = Get-Content -LiteralPath (Join-Path $RunnerDirectory '.runner') -Raw | ConvertFrom-Json
                if ($failedRegistration.gitHubUrl -eq 'https://github.com/David-Martel/agent-hub') {
                    Write-RunnerReadiness -Directory $RunnerDirectory -Receipt @{
                        AtUtc = [DateTime]::UtcNow.ToString('o'); SupervisorPid = $PID
                        SessionId = (Get-Process -Id $PID).SessionId; Ready = $false
                        ErrorType = $failure.Exception.GetType().Name; Error = $failure.Exception.Message
                    }
                }
            } catch { Write-Warning 'Could not persist the owned runner failure receipt' }
        }
        Write-Error -ErrorRecord $failure -ErrorAction Continue
        exit 1
    }
}
