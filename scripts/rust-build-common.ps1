Set-StrictMode -Version Latest

function Initialize-AgentBusSccacheServer {
    param(
        [Parameter(Mandatory = $true)]
        [string]$SccachePath,
        [switch]$ResetStats
    )

    $needsRestart = $false

    try {
        $probeOutput = & $SccachePath --show-stats 2>&1
        if ($LASTEXITCODE -ne 0 -or ($probeOutput -join "`n") -match "Mismatch of client/server versions") {
            $needsRestart = $true
        }
    }
    catch {
        $needsRestart = $true
    }

    if ($needsRestart) {
        try {
            & $SccachePath --stop-server *> $null
        }
        catch {
        }
    }

    try {
        & $SccachePath --start-server *> $null
    }
    catch {
    }

    if ($ResetStats) {
        try {
            & $SccachePath --zero-stats *> $null
        }
        catch {
        }
    }
}

function Get-AgentBusCommandPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [string[]]$Candidates = @()
    )

    foreach ($candidate in $Candidates) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and (Test-Path $candidate)) {
            return (Resolve-Path $candidate).Path
        }
    }

    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    return $null
}

function Resolve-AgentBusCargoTargetRoot {
    param(
        [string]$RepoRoot,
        [string]$ExplicitTargetRoot
    )

    if ($ExplicitTargetRoot) {
        return $ExplicitTargetRoot
    }

    if ($env:AGENT_BUS_CARGO_TARGET_ROOT) {
        return $env:AGENT_BUS_CARGO_TARGET_ROOT
    }

    if ($env:CARGO_TARGET_DIR) {
        return $env:CARGO_TARGET_DIR
    }

    if (Test-Path "T:\RustCache\cargo-target") {
        return "T:\RustCache\cargo-target"
    }

    if ($env:USERPROFILE) {
        return Join-Path $env:USERPROFILE ".cache\agent-bus\cargo-target"
    }

    return Join-Path $RepoRoot ".cargo-target"
}

function Resolve-AgentBusTargetDir {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepoRoot,
        [string]$ExplicitTargetDir,
        [string]$ExplicitNamespace,
        [string]$TargetRoot
    )

    if ($ExplicitTargetDir) {
        return $ExplicitTargetDir
    }

    $resolvedRoot = Resolve-AgentBusCargoTargetRoot -RepoRoot $RepoRoot -ExplicitTargetRoot $TargetRoot
    $namespace = if ($ExplicitNamespace) {
        $ExplicitNamespace
    }
    else {
        "agent-bus-build-{0}-{1}" -f $PID, (Get-Date -Format "yyyyMMdd-HHmmss")
    }

    return Join-Path $resolvedRoot $namespace
}

function Use-AgentBusRustBuildEnv {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepoRoot,
        [Parameter(Mandatory = $true)]
        [string]$TargetDir,
        [switch]$PreferSccache,
        [switch]$PreferLldLink,
        [switch]$PreferFastLink,
        [switch]$EnableIncremental,
        [switch]$ResetSccacheStats,
        [switch]$ShowSummary
    )

    $state = @{
        Snapshot = @{
            CARGO_TARGET_DIR = $env:CARGO_TARGET_DIR
            RUSTC_WRAPPER = $env:RUSTC_WRAPPER
            CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER
            RUSTFLAGS = $env:RUSTFLAGS
            CARGO_INCREMENTAL = $env:CARGO_INCREMENTAL
        }
        Summary = [ordered]@{
            TargetDir = $TargetDir
            Sccache = $null
            Linker = $null
            RustFlags = $null
            Incremental = $false
        }
    }

    $env:CARGO_TARGET_DIR = $TargetDir
    New-Item -ItemType Directory -Path $TargetDir -Force | Out-Null

    $incrementalRequested = $EnableIncremental.IsPresent
    if ($PreferSccache) {
        $sccache = Get-AgentBusCommandPath -Name "sccache" -Candidates @(
            (Join-Path $env:USERPROFILE ".cargo\bin\sccache.exe"),
            (Join-Path $env:USERPROFILE "bin\sccache.exe")
        )
        if ($sccache) {
            $env:RUSTC_WRAPPER = $sccache
            Initialize-AgentBusSccacheServer -SccachePath $sccache -ResetStats:$ResetSccacheStats
            $state.Summary.Sccache = $sccache
        }
    }

    if ($incrementalRequested -and -not $state.Summary.Sccache) {
        $env:CARGO_INCREMENTAL = "1"
        $state.Summary.Incremental = $true
    }
    else {
        Remove-Item Env:CARGO_INCREMENTAL -ErrorAction SilentlyContinue
        $state.Summary.Incremental = $false
    }

    if ($PreferLldLink) {
        $lldLink = Get-AgentBusCommandPath -Name "lld-link" -Candidates @(
            "C:\Program Files\LLVM\bin\lld-link.exe"
        )
        if ($lldLink) {
            $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $lldLink
            $state.Summary.Linker = $lldLink
        }
    }

    if ($PreferFastLink) {
        $fastLinkFlag = "-C link-arg=/DEBUG:FASTLINK"
        if ([string]::IsNullOrWhiteSpace($env:RUSTFLAGS)) {
            $env:RUSTFLAGS = $fastLinkFlag
        }
        elseif ($env:RUSTFLAGS -notmatch [regex]::Escape($fastLinkFlag)) {
            $env:RUSTFLAGS = "$($env:RUSTFLAGS.Trim()) $fastLinkFlag"
        }
        $state.Summary.RustFlags = $env:RUSTFLAGS
    }
    else {
        $state.Summary.RustFlags = $env:RUSTFLAGS
    }

    if ($ShowSummary) {
        Write-Host "Rust build environment:"
        Write-Host "  CARGO_TARGET_DIR: $($state.Summary.TargetDir)"
        Write-Host "  RUSTC_WRAPPER:    $(if ($state.Summary.Sccache) { $state.Summary.Sccache } else { '<none>' })"
        Write-Host "  LINKER:           $(if ($state.Summary.Linker) { $state.Summary.Linker } else { '<default>' })"
        Write-Host "  RUSTFLAGS:        $(if ($state.Summary.RustFlags) { $state.Summary.RustFlags } else { '<none>' })"
        Write-Host "  CARGO_INCREMENTAL: $(if ($state.Summary.Incremental) { '1' } else { $(if ($env:CARGO_INCREMENTAL) { $env:CARGO_INCREMENTAL } else { '<default>' }) })"
    }

    return $state
}

function Restore-AgentBusRustBuildEnv {
    param(
        [Parameter(Mandatory = $true)]
        [hashtable]$State
    )

    foreach ($name in @(
        "CARGO_TARGET_DIR",
        "RUSTC_WRAPPER",
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
        "RUSTFLAGS",
        "CARGO_INCREMENTAL"
    )) {
        $value = $State.Snapshot[$name]
        if ($null -eq $value -or $value -eq "") {
            Remove-Item "Env:$name" -ErrorAction SilentlyContinue
        }
        else {
            Set-Item "Env:$name" $value
        }
    }
}

function Write-AgentBusSccacheStats {
    $sccache = if ($env:RUSTC_WRAPPER -and (Split-Path $env:RUSTC_WRAPPER -Leaf) -like "sccache*") {
        $env:RUSTC_WRAPPER
    }
    else {
        Get-AgentBusCommandPath -Name "sccache" -Candidates @(
            (Join-Path $env:USERPROFILE ".cargo\bin\sccache.exe"),
            (Join-Path $env:USERPROFILE "bin\sccache.exe")
        )
    }

    if (-not $sccache) {
        return
    }

    try {
        $statsOutput = & $sccache --show-stats 2>&1
        if ($LASTEXITCODE -eq 0) {
            Write-Host "`nSccache stats:"
            $statsOutput
            return
        }

        $joinedOutput = ($statsOutput -join "`n").Trim()
        if ($joinedOutput -match "Mismatch of client/server versions") {
            Write-Host "`nSccache stats unavailable: restarted server version differs from the prior resident server."
            return
        }

        Write-Warning "Could not read sccache stats: $joinedOutput"
    }
    catch {
        Write-Warning "Could not read sccache stats: $($_.Exception.Message)"
    }
}

function Find-AgentBusBinary {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Candidates
    )

    return $Candidates |
        Where-Object { Test-Path $_ } |
        Sort-Object { (Get-Item $_).LastWriteTimeUtc } -Descending |
        Select-Object -First 1
}

function Find-AgentBusBuiltBinary {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RustCliDir,
        [Parameter(Mandatory = $true)]
        [string]$TargetDir,
        [Parameter(Mandatory = $true)]
        [string]$BinaryName,
        [string]$Profile = "release"
    )

    $candidates = @(
        (Join-Path $TargetDir "$Profile\$BinaryName.exe"),
        "T:\RustCache\cargo-target\$Profile\$BinaryName.exe",
        (Join-Path $RustCliDir "target\$Profile\$BinaryName.exe")
    )

    return Find-AgentBusBinary -Candidates $candidates
}
