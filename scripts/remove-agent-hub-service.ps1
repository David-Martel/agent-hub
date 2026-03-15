param(
    [string]$ServiceName = "AgentHub"
)

$ErrorActionPreference = "Stop"
$nssmPath = (Get-Command nssm -ErrorAction SilentlyContinue).Source

$service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if (-not $service) {
    Write-Host "Service '$ServiceName' does not exist."
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
