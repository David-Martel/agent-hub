#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Configure log rotation for the AgentHub Windows service (nssm).
.DESCRIPTION
    Sets nssm AppRotate* parameters to enable automatic log rotation
    by file size and/or time interval.
.PARAMETER ServiceName
    Name of the nssm-managed service. Default: AgentHub
.PARAMETER LogDir
    Directory for log files. Default: C:\ProgramData\AgentHub\logs
.PARAMETER MaxFileSizeKB
    Rotate when log exceeds this size in KB. Default: 10240 (10 MB)
.PARAMETER RotateSeconds
    Rotate after this many seconds. Default: 86400 (daily)
#>
param(
    [string]$ServiceName = "AgentHub",
    [string]$LogDir = "C:\ProgramData\AgentHub\logs",
    [int]$MaxFileSizeKB = 10240,
    [int]$RotateSeconds = 86400
)

$ErrorActionPreference = "Stop"

if (-not (Get-Command nssm -ErrorAction SilentlyContinue)) {
    Write-Error "nssm not found in PATH. Install it first: https://nssm.cc/"
    exit 1
}

# Verify service exists
$status = nssm status $ServiceName 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Error "Service '$ServiceName' not found. Install it first with scripts/install-agent-hub-service.ps1"
    exit 1
}

# Ensure log directory exists
if (-not (Test-Path $LogDir)) {
    New-Item -ItemType Directory -Path $LogDir -Force | Out-Null
    Write-Host "Created log directory: $LogDir"
}

# Configure stdout/stderr log paths
nssm set $ServiceName AppStdout "$LogDir\agent-hub-service.log"
nssm set $ServiceName AppStderr "$LogDir\agent-hub-service-error.log"

# Enable log rotation
nssm set $ServiceName AppRotateFiles 1
nssm set $ServiceName AppRotateOnline 1
nssm set $ServiceName AppRotateSeconds $RotateSeconds
nssm set $ServiceName AppRotateBytes ($MaxFileSizeKB * 1024)

Write-Host ""
Write-Host "Log rotation configured for '$ServiceName':"
Write-Host "  Log directory:  $LogDir"
Write-Host "  Max file size:  $MaxFileSizeKB KB"
Write-Host "  Rotate interval: $RotateSeconds seconds"
Write-Host ""
Write-Host "Restart the service for changes to take effect:"
Write-Host "  nssm restart $ServiceName"
