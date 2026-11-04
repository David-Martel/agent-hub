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
    [int]$KeepNewest = 3,
    # Test seam: lets a test simulate a disk that returns different bytes.
    [scriptblock]$DiskHashCommand = { param($p) [HubBackup.DiskHash]::Compute($p) }
)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$log = Join-Path $Dest 'backup.log'

function Write-BackupLog([string]$Text) {
    [IO.File]::AppendAllText($log, ('{0} {1}{2}' -f (Get-Date).ToUniversalTime().ToString('s'), $Text, "`r`n"))
}

# FILE_FLAG_NO_BUFFERING needs a sector-aligned buffer, which a managed array doesn't guarantee. VirtualAlloc returns
# page-aligned memory, and every read is a whole multiple of the block size.
if (-not ('HubBackup.DiskHash' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using Microsoft.Win32.SafeHandles;

namespace HubBackup {
    public static class DiskHash {
        const uint GenericRead = 0x80000000, ShareRead = 1, OpenExisting = 3;
        const uint NoBuffering = 0x20000000, SequentialScan = 0x08000000;
        const uint MemCommit = 0x1000, MemReserve = 0x2000, MemRelease = 0x8000, PageReadWrite = 4;
        const int Block = 1024 * 1024;

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool ReadFile(SafeFileHandle file, IntPtr buffer, int count, out int read, IntPtr overlapped);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern IntPtr VirtualAlloc(IntPtr address, UIntPtr size, uint type, uint protect);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool VirtualFree(IntPtr address, UIntPtr size, uint type);

        public static string Compute(string path) {
            IntPtr native = VirtualAlloc(IntPtr.Zero, (UIntPtr)Block, MemCommit | MemReserve, PageReadWrite);
            if (native == IntPtr.Zero) throw new Win32Exception();
            try {
                using (SafeFileHandle file = CreateFileW(path, GenericRead, ShareRead, IntPtr.Zero, OpenExisting, NoBuffering | SequentialScan, IntPtr.Zero))
                using (SHA256 sha = SHA256.Create()) {
                    if (file.IsInvalid) throw new Win32Exception();
                    byte[] managed = new byte[Block];
                    int read;
                    while (true) {
                        if (!ReadFile(file, native, Block, out read, IntPtr.Zero)) throw new Win32Exception();
                        if (read == 0) break;
                        Marshal.Copy(native, managed, 0, read);
                        sha.TransformBlock(managed, 0, read, null, 0);
                    }
                    sha.TransformFinalBlock(new byte[0], 0, 0);
                    return BitConverter.ToString(sha.Hash).Replace("-", "");
                }
            } finally { VirtualFree(native, UIntPtr.Zero, MemRelease); }
        }
    }
}
'@
}

# Flush the file, then compare what the producer wrote (still in the page cache) with what reached the disk. A mismatch
# is the failure this script exists to catch, so it stops the run before anything is published or pruned.
function Assert-OnDisk([string]$Path) {
    $handle = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::Read)
    try { $handle.Flush($true) } finally { $handle.Dispose() }
    $written = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    $onDisk = & $DiskHashCommand $Path
    if ($written -ne $onDisk) { throw "disk copy differs from what was written: $(Split-Path $Path -Leaf)" }
    $onDisk
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
        '{0} {1} {2}' -f (Split-Path $path -Leaf), (Get-Item $path).Length, (Assert-OnDisk $path)
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
