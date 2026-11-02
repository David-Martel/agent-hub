<#
.SYNOPSIS
Prune old agent-hub history bundles from the SFTP drop, keeping the newest N and
every bundle another artifact references. Logs every removal as NDJSON.

.DESCRIPTION
Runs as the dedicated local account agenthubprune from the scheduled task that
Install-AgentHubPrune.ps1 registers, never as the drop account (agenthubdrop),
which by design cannot delete anything. See ../README.md, "Retention".

Kept, never deleted:
  - the newest -Keep bundles, ordered by directory creation time. The filesystem
    sets that time; a MANIFEST's snapshot time is content the uploader controls,
    so it is logged but never used to order;
  - every bundle whose name appears in a receipt (*.receipt.json) or a
    MANIFEST.json anywhere under -ArtifactsRoot, other than its own receipt
    and its own manifest, or in the pin file -Root\KEEP (one name per line);
  - every bundle that fails verification (it is logged as an anomaly instead,
    because a damaged bundle is evidence and its name no longer proves content).

Before each delete the bundle is verified: its name suffix must equal the first
16 hex of sha256(SHA256SUMS), every SHA256SUMS line must match, and no unlisted
file may be present. The log line carrying those hashes is written before the
delete. The bundle's own receipt is removed with it, and its hash is logged.

Nothing is deleted when -WhatIf is given, or when the drop holds -Keep bundles
or fewer.
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$Root = 'C:\codedev\agent-hub-artifacts\incoming',
    [string]$ArtifactsRoot = 'C:\codedev\agent-hub-artifacts',
    [string]$LogPath = 'C:\codedev\agent-hub-artifacts\prune-log\prune.ndjson',
    [ValidateRange(1, 10000)][int]$Keep = 14
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$namePattern = '^history-[0-9a-f]{16}$'
$refPattern = 'history-[0-9a-f]{16}'
$runId = [guid]::NewGuid().ToString()

function Write-PruneLog([hashtable]$Entry) {
    $Entry['run'] = $runId
    $Entry['utc'] = [DateTime]::UtcNow.ToString('o')
    # .NET calls, not New-Item: -WhatIf must still write the would-remove log.
    [void][IO.Directory]::CreateDirectory((Split-Path -Parent $LogPath))
    $line = ($Entry | ConvertTo-Json -Compress -Depth 5)
    [IO.File]::AppendAllText($LogPath, $line + "`n", (New-Object Text.UTF8Encoding($false)))
    $line
}

function Get-Sha256([string]$Path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant() }

function Test-Bundle([IO.DirectoryInfo]$Dir) {
    # Returns @{ ok; reason; sums; files; bytes }.
    $sums = Join-Path $Dir.FullName 'SHA256SUMS'
    if (-not (Test-Path -LiteralPath $sums -PathType Leaf)) { return @{ ok = $false; reason = 'no SHA256SUMS' } }
    $sumsHash = Get-Sha256 $sums
    if ($Dir.Name -ne ('history-' + $sumsHash.Substring(0, 16))) {
        return @{ ok = $false; reason = 'name does not match sha256(SHA256SUMS)'; sums = $sumsHash }
    }
    $listed = @{}
    foreach ($line in [IO.File]::ReadAllLines($sums)) {
        if (-not $line.Trim()) { continue }
        $parts = $line -split '  ', 2
        if ($parts.Count -ne 2) { return @{ ok = $false; reason = 'malformed SHA256SUMS line'; sums = $sumsHash } }
        $listed[$parts[1]] = $parts[0].ToLowerInvariant()
    }
    $bytes = 0
    $onDisk = @(Get-ChildItem -LiteralPath $Dir.FullName -Recurse -File -Force)
    foreach ($f in $onDisk) {
        $rel = $f.FullName.Substring($Dir.FullName.Length + 1).Replace('\', '/')
        if ($rel -eq 'SHA256SUMS') { continue }
        if (-not $listed.ContainsKey($rel)) { return @{ ok = $false; reason = "unlisted file $rel"; sums = $sumsHash } }
        if ((Get-Sha256 $f.FullName) -ne $listed[$rel]) { return @{ ok = $false; reason = "hash mismatch $rel"; sums = $sumsHash } }
        $bytes += $f.Length
    }
    if ($onDisk.Count - 1 -ne $listed.Count) { return @{ ok = $false; reason = 'listed file missing'; sums = $sumsHash } }
    return @{ ok = $true; reason = 'verified'; sums = $sumsHash; files = $onDisk.Count; bytes = $bytes }
}

function Get-Snapshot([IO.DirectoryInfo]$Dir) {
    # For the log only; see the ordering note in the help text.
    try {
        return [string](Get-Content -Raw -LiteralPath (Join-Path $Dir.FullName 'MANIFEST.json') | ConvertFrom-Json).pg_snapshot_utc
    } catch { return $null }
}

if (-not (Test-Path -LiteralPath $Root -PathType Container)) { throw "drop root $Root does not exist" }
$bundles = @(Get-ChildItem -LiteralPath $Root -Directory -Force | Where-Object { $_.Name -match $namePattern })

# References: names found in receipts and manifests, excluding each file's self-reference.
$referenced = @{}
$refFiles = @(Get-ChildItem -LiteralPath $ArtifactsRoot -Recurse -File -Force -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like '*.receipt.json' -or $_.Name -eq 'MANIFEST.json' })
foreach ($rf in $refFiles) {
    $self = if ($rf.Name -like '*.receipt.json') { $rf.Name.Substring(0, $rf.Name.Length - '.receipt.json'.Length) }
            else { $rf.Directory.Name }
    foreach ($m in [regex]::Matches([IO.File]::ReadAllText($rf.FullName), $refPattern)) {
        if ($m.Value -ne $self) { $referenced[$m.Value] = $rf.FullName }
    }
}
$pinFile = Join-Path $Root 'KEEP'
if (Test-Path -LiteralPath $pinFile -PathType Leaf) {
    foreach ($line in [IO.File]::ReadAllLines($pinFile)) {
        $n = $line.Trim()
        if ($n -match $namePattern) { $referenced[$n] = $pinFile }
    }
}

$ordered = @($bundles | Sort-Object -Property CreationTimeUtc, Name -Descending)
$newest = @($ordered | Select-Object -First $Keep | ForEach-Object Name)
$summary = @{ event = 'summary'; root = $Root; keep = $Keep; bundles = $bundles.Count; kept_newest = $newest.Count;
              kept_referenced = 0; anomalies = 0; removed = 0; removed_bytes = 0; whatif = [bool]$WhatIfPreference }

if ($bundles.Count -gt $Keep) {
    foreach ($b in ($ordered | Select-Object -Skip $Keep)) {
        if ($referenced.ContainsKey($b.Name)) {
            $summary.kept_referenced++
            Write-PruneLog @{ event = 'kept-referenced'; bundle = $b.Name; by = $referenced[$b.Name] } | Out-Null
            continue
        }
        $v = Test-Bundle $b
        if (-not $v.ok) {
            $summary.anomalies++
            Write-PruneLog @{ event = 'anomaly-kept'; bundle = $b.Name; reason = $v.reason; sha256sums = $v.sums } | Out-Null
            continue
        }
        $receipt = Join-Path $Root ($b.Name + '.receipt.json')
        $receiptHash = if (Test-Path -LiteralPath $receipt -PathType Leaf) { Get-Sha256 $receipt } else { $null }
        if ($PSCmdlet.ShouldProcess($b.FullName, 'remove verified bundle')) {
            Write-PruneLog @{ event = 'remove'; bundle = $b.Name; sha256sums = $v.sums; files = $v.files;
                              bytes = $v.bytes; snapshot = (Get-Snapshot $b); created = $b.CreationTimeUtc.ToString('o'); receipt_sha256 = $receiptHash } | Out-Null
            try {
                # .NET deletes, not Remove-Item -Force: under this account Remove-Item was denied
                # on dtm-p1gen7 (2026-10-29) while these succeed. The likely cause is that -Force
                # resets attributes first, which needs WriteAttributes, a right withheld on purpose.
                [IO.Directory]::Delete($b.FullName, $true)
                if ($receiptHash) { [IO.File]::Delete($receipt) }
            } catch {
                # The remove line above is already written, so record that it did not happen
                # and fail the run (the task's last result shows it).
                Write-PruneLog @{ event = 'remove-failed'; bundle = $b.Name; error = $_.Exception.Message;
                                  still_present = (Test-Path -LiteralPath $b.FullName) } | Out-Null
                throw
            }
            $summary.removed++
            $summary.removed_bytes += $v.bytes
        } else {
            Write-PruneLog @{ event = 'would-remove'; bundle = $b.Name; sha256sums = $v.sums; files = $v.files;
                              bytes = $v.bytes } | Out-Null
        }
    }
}
Write-PruneLog $summary
