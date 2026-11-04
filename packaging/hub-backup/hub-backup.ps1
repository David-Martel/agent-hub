<#
.SYNOPSIS
    Daily local backup of a Windows-hosted agent hub: a PostgreSQL dump and a Redis snapshot.
.DESCRIPTION
    Writes to a .partial folder and renames it only after every check passes, so a dated folder is always complete.
    The manifest hashes through an uncached read, because a hash taken right after writing can come from the page
    cache and pass while the copy on disk is wrong. That happened on dtm-work's F: drive on 2026-11-04.
    Off-site custody is packaging/history-export, not this script.
#>
[CmdletBinding()]
param(
    [string]$Dest = 'R:\AgentHub-backups',
    [string]$PgBin = 'C:\Program Files\PostgreSQL\18\pgsql\bin',
    [string]$RedisBin = 'C:\Program Files\Redis',
    [int]$PgPort = 5432,
    [int]$RedisPort = 6380,
    [int]$RetentionDays = 14,
    [int]$KeepNewest = 3
)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$log = Join-Path $Dest 'backup.log'

function Write-BackupLog([string]$Text) {
    [IO.File]::AppendAllText($log, ('{0} {1}{2}' -f (Get-Date).ToUniversalTime().ToString('s'), $Text, "`r`n"))
}

function Get-DiskHash([string]$Path) {
    $sha = [Security.Cryptography.SHA256]::Create()
    $block = 1MB
    $buffer = New-Object byte[] $block
    # 0x20000000 is FILE_FLAG_NO_BUFFERING. It reads the disk, not the page cache.
    $options = [IO.FileOptions]([int][IO.FileOptions]::SequentialScan -bor 0x20000000)
    $stream = New-Object IO.FileStream($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read, 1, $options)
    try {
        while (($count = $stream.Read($buffer, 0, $block)) -gt 0) { [void]$sha.TransformBlock($buffer, 0, $count, $null, 0) }
        [void]$sha.TransformFinalBlock(@(), 0, 0)
    } finally { $stream.Dispose() }
    [BitConverter]::ToString($sha.Hash) -replace '-', ''
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$partial = Join-Path $Dest ".partial-$stamp"
$final = Join-Path $Dest $stamp
try {
    if ((Test-Path $partial) -or (Test-Path $final)) { throw "backup path already exists: $stamp" }
    New-Item -ItemType Directory $partial | Out-Null
    $dump = Join-Path $partial 'redis_backend.dump'
    $rdb = Join-Path $partial 'redis.rdb'
    & (Join-Path $PgBin 'pg_dump') -w -h 127.0.0.1 -p $PgPort -U postgres -Fc -f $dump redis_backend
    if ($LASTEXITCODE -ne 0) { throw "pg_dump exit $LASTEXITCODE" }
    & (Join-Path $RedisBin 'redis-cli') -p $RedisPort --rdb $rdb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "redis snapshot exit $LASTEXITCODE" }
    & (Join-Path $PgBin 'pg_restore') --list $dump | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "dump unreadable, exit $LASTEXITCODE" }
    # redis-check-rdb reports on stderr, which Stop would turn into a terminating error.
    $ErrorActionPreference = 'Continue'
    $check = @(& (Join-Path $RedisBin 'redis-check-rdb') $rdb 2>&1)
    $checkExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($checkExit -ne 0 -or @($check | Where-Object { "$_" -match 'Terminating|CRC error|Wrong' }).Count -gt 0) {
        throw "redis-check-rdb failed, exit $checkExit"
    }
    # The manifest names closed producer files only, so it can never hash itself.
    $lines = foreach ($path in @($dump, $rdb)) {
        '{0} {1} {2}' -f (Split-Path $path -Leaf), (Get-Item $path).Length, (Get-DiskHash $path)
    }
    [IO.File]::WriteAllLines((Join-Path $partial 'SHA256SUMS.txt'), [string[]]$lines, (New-Object Text.UTF8Encoding($false)))
    Rename-Item $partial $final
    Write-BackupLog "ok $stamp $($lines -join ' | ')"
    # Age out old runs only after a verified success, and always keep the newest few.
    $runs = @(Get-ChildItem $Dest -Directory | Where-Object { $_.Name -match '^\d{8}-\d{6}$' } | Sort-Object Name -Descending)
    $runs | Select-Object -Skip $KeepNewest | Where-Object { $_.CreationTime -lt (Get-Date).AddDays(-$RetentionDays) } | Remove-Item -Recurse -Force
} catch {
    Write-BackupLog "FAILED $stamp $($_.Exception.Message)"
    throw
}
