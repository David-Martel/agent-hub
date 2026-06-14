param(
    [string]$ServiceName = "AgentHub",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$nssmPath = (Get-Command nssm -ErrorAction SilentlyContinue).Source

$service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if (-not $service) {
    Write-Host "Service '$ServiceName' does not exist."
    exit 0
}

if ($DryRun) {
    Write-Host "[DRY-RUN] Service removal plan:" -ForegroundColor Cyan
    Write-Host "  - Service name: $ServiceName"
    Write-Host "  - Current status: $($service.Status)"
    if ($service.Status -ne "Stopped") {
        Write-Host "  - Stop service with Stop-Service -Force"
    }
    if ($nssmPath) {
        Write-Host "  - Remove service with: $nssmPath remove $ServiceName confirm"
    }
    else {
        Write-Host "  - Remove service with: sc.exe delete $ServiceName"
    }
    Write-Host "  No service state was changed."
    exit 0
}

if ($service.Status -ne "Stopped") {
    Stop-Service -Name $ServiceName -Force
    $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
}

if ($nssmPath) {
    & $nssmPath remove $ServiceName confirm | Out-Null
}
else {
    & sc.exe delete $ServiceName | Out-Null
}
Write-Host "Removed service '$ServiceName'."
