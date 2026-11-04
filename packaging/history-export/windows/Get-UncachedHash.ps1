<#
.SYNOPSIS
    SHA-256 and length of one file, read past the Windows file cache.

.DESCRIPTION
    A hash taken right after writing can be answered from cache while the stored
    copy is wrong, so it does not prove the stored bytes. This opens the file with
    FILE_FLAG_NO_BUFFERING (0x20000000), the same read AgentHub-daily-backup's
    Get-DiskHash uses, and prints one JSON object: {"sha256": "<lower hex>",
    "bytes": <length>}. It reads and writes nothing else. The history exporter
    runs it through WSL interop (uncached.mode = "command", README "Work-authoritative profile").
#>
param([Parameter(Mandatory = $true)][string]$Path)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$sha = [Security.Cryptography.SHA256]::Create()
$block = 1MB
$buffer = New-Object byte[] $block
$total = [long]0
$options = [IO.FileOptions]([int][IO.FileOptions]::SequentialScan -bor 0x20000000)
$stream = New-Object IO.FileStream($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read, 1, $options)
try {
    while (($n = $stream.Read($buffer, 0, $block)) -gt 0) {
        [void]$sha.TransformBlock($buffer, 0, $n, $null, 0)
        $total += $n
    }
    [void]$sha.TransformFinalBlock(@(), 0, 0)
} finally {
    $stream.Dispose()
}
$hex = ([BitConverter]::ToString($sha.Hash) -replace '-', '').ToLowerInvariant()
'{{"sha256": "{0}", "bytes": {1}}}' -f $hex, $total
