<#
.SYNOPSIS
    SHA-256 and length of one file, read past the Windows file cache.

.DESCRIPTION
    A hash taken right after writing can be answered from cache while the stored
    copy is wrong, so it does not prove the stored bytes. This reads with
    FILE_FLAG_NO_BUFFERING, which needs a sector-aligned buffer: a managed array
    does not guarantee that, so the buffer is page-aligned VirtualAlloc memory and
    every read is a whole multiple of the block size (the same approach as
    HubBackup.DiskHash in packaging/hub-backup; this script is self-contained on
    purpose). It prints one JSON object, {"sha256": "<lower hex>", "bytes": <length>},
    and reads and writes nothing else. The history exporter runs it through WSL
    interop (uncached.mode = "command") and compares the result with the hash it
    took while writing; the comparison, not this script, decides.
#>
param([Parameter(Mandatory = $true)][string]$Path)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if (-not ('HistoryExport.UncachedHash' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using Microsoft.Win32.SafeHandles;

namespace HistoryExport {
    public static class UncachedHash {
        const uint GenericRead = 0x80000000, ShareReadWrite = 3, OpenExisting = 3;
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

        public static string Compute(string path, out long length) {
            length = 0;
            IntPtr native = VirtualAlloc(IntPtr.Zero, (UIntPtr)Block, MemCommit | MemReserve, PageReadWrite);
            if (native == IntPtr.Zero) throw new Win32Exception();
            try {
                using (SafeFileHandle file = CreateFileW(path, GenericRead, ShareReadWrite, IntPtr.Zero, OpenExisting, NoBuffering | SequentialScan, IntPtr.Zero))
                using (SHA256 sha = SHA256.Create()) {
                    if (file.IsInvalid) throw new Win32Exception();
                    byte[] managed = new byte[Block];
                    int read;
                    while (true) {
                        if (!ReadFile(file, native, Block, out read, IntPtr.Zero)) throw new Win32Exception();
                        if (read == 0) break;
                        Marshal.Copy(native, managed, 0, read);
                        sha.TransformBlock(managed, 0, read, null, 0);
                        length += read;
                    }
                    sha.TransformFinalBlock(new byte[0], 0, 0);
                    return BitConverter.ToString(sha.Hash).Replace("-", "").ToLowerInvariant();
                }
            } finally { VirtualFree(native, UIntPtr.Zero, MemRelease); }
        }
    }
}
'@
}

[long]$total = 0
$hex = [HistoryExport.UncachedHash]::Compute($Path, [ref]$total)
'{{"sha256": "{0}", "bytes": {1}}}' -f $hex, $total
