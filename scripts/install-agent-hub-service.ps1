param(
    [string]$ServiceName = "AgentHub",
    [string]$DisplayName = "Agent Hub",
    [int]$Port = 8400,
    [string]$DatabaseName = "redis_backend",
    [switch]$ForceReinstall,
    [switch]$SkipBuild,
    [switch]$StartService = $true
)

$ErrorActionPreference = "Stop"

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptRoot
$setupScript = Join-Path $scriptRoot "setup-agent-hub-local.ps1"
$runnerScript = Join-Path $scriptRoot "run-agent-hub-service.ps1"
$pwshPath = (Get-Command pwsh -ErrorAction Stop).Source
$nssmPath = (Get-Command nssm -ErrorAction Stop).Source
$logRoot = "C:\ProgramData\AgentHub\logs"
$stdoutLog = Join-Path $logRoot "agent-hub-stdout.log"
$stderrLog = Join-Path $logRoot "agent-hub-stderr.log"

New-Item -ItemType Directory -Path $logRoot -Force | Out-Null

& $setupScript -DatabaseName $DatabaseName -SkipHealth:$false -SkipBuild:$SkipBuild
if ($LASTEXITCODE -ne 0) {
    throw "setup-agent-hub-local.ps1 failed."
}

$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing) {
    if (-not $ForceReinstall) {
        throw "Service '$ServiceName' already exists. Use -ForceReinstall to replace it."
    }
    if ($existing.Status -ne "Stopped") {
        Stop-Service -Name $ServiceName -Force
        $existing.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    }
    & $nssmPath remove $ServiceName confirm | Out-Null
    Start-Sleep -Seconds 2
}

& $nssmPath install $ServiceName $pwshPath "-NoLogo" "-NoProfile" "-ExecutionPolicy" "Bypass" "-File" $runnerScript "-Port" $Port "-DatabaseName" $DatabaseName | Out-Null
& $nssmPath set $ServiceName DisplayName $DisplayName | Out-Null
& $nssmPath set $ServiceName Description "Rust-native Agent Hub HTTP service backed by local Redis and PostgreSQL." | Out-Null
& $nssmPath set $ServiceName Start SERVICE_AUTO_START | Out-Null
& $nssmPath set $ServiceName AppDirectory $repoRoot | Out-Null
& $nssmPath set $ServiceName AppStdout $stdoutLog | Out-Null
& $nssmPath set $ServiceName AppStderr $stderrLog | Out-Null
& $nssmPath set $ServiceName AppRotateFiles 1 | Out-Null
& $nssmPath set $ServiceName AppRotateOnline 1 | Out-Null
& $nssmPath set $ServiceName AppRotateSeconds 86400 | Out-Null
& $nssmPath set $ServiceName AppRotateBytes 10485760 | Out-Null
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null

if ($StartService) {
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus("Running", [TimeSpan]::FromSeconds(30))

    $healthUri = "http://localhost:$Port/health"
    $deadline = (Get-Date).AddSeconds(30)
    do {
        try {
            $health = Invoke-RestMethod -Uri $healthUri -Method Get -TimeoutSec 5
            if ($health.ok -eq $true) {
                Write-Host ($health | ConvertTo-Json -Depth 10)
                return
            }
        }
        catch {
            Start-Sleep -Seconds 2
        }
    } while ((Get-Date) -lt $deadline)

    throw "Service '$ServiceName' started but health endpoint did not become ready at $healthUri."
}
