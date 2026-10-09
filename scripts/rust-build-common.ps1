Set-StrictMode -Version Latest

$script:AgentBusDisableSccacheForCargoSteps = $false

function Initialize-AgentBusSccacheServer {
    param(
        [Parameter(Mandatory = $true)]
        [string]$SccachePath,
        [switch]$ResetStats
    )

    try {
        $probeOutput = & $SccachePath --show-stats 2>&1
        if ($LASTEXITCODE -ne 0 -or ($probeOutput -join "`n") -match "Mismatch of client/server versions") {
            Write-Warning "sccache is unavailable or version-mismatched; leaving the shared daemon untouched."
            return $false
        }
    }
    catch {
        Write-Warning "sccache health probe failed; leaving the shared daemon untouched: $($_.Exception.Message)"
        return $false
    }

    if ($ResetStats) {
        try {
            & $SccachePath --zero-stats *> $null
        }
        catch {
            Write-Warning "Could not reset shared sccache statistics; continuing without changing daemon state."
        }
    }

    return $true
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
            RUSTC_WORKSPACE_WRAPPER = $env:RUSTC_WORKSPACE_WRAPPER
            CARGO_BUILD_RUSTC_WRAPPER = $env:CARGO_BUILD_RUSTC_WRAPPER
            CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER = $env:CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER
            CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER
            RUSTFLAGS = $env:RUSTFLAGS
            CARGO_INCREMENTAL = $env:CARGO_INCREMENTAL
        }
        DisableSccacheForCargoSteps = $script:AgentBusDisableSccacheForCargoSteps
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

    # An explicitly false preference is the caller's uncached build request.
    # Omitting it preserves unrelated custom wrappers and existing scope state.
    if ($PSBoundParameters.ContainsKey('PreferSccache')) {
        if ($PreferSccache) { $script:AgentBusDisableSccacheForCargoSteps = $false }
        else { Disable-AgentBusSccacheForCargoSteps }
    }
    $incrementalRequested = $EnableIncremental.IsPresent
    if ($PreferSccache) {
        $sccache = Get-AgentBusCommandPath -Name "sccache" -Candidates @(
            (Join-Path $env:USERPROFILE ".cargo\bin\sccache.exe"),
            (Join-Path $env:USERPROFILE "bin\sccache.exe")
        )
        if ($sccache) {
            if (Initialize-AgentBusSccacheServer -SccachePath $sccache -ResetStats:$ResetSccacheStats) {
                $env:RUSTC_WRAPPER = $sccache
                $state.Summary.Sccache = $sccache
            }
            else {
                Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue
                Write-Warning "sccache is unavailable or unhealthy; continuing without RUSTC_WRAPPER."
            }
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
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
        "RUSTFLAGS",
        "CARGO_INCREMENTAL"
    )) {
        # Empty wrapper values intentionally suppress Cargo configuration;
        # restore them distinctly from an absent variable.
        if ($null -eq $State.Snapshot[$name]) {
            Remove-Item "Env:$name" -ErrorAction SilentlyContinue
        } else {
            Set-Item "Env:$name" $State.Snapshot[$name]
        }
    }
    $script:AgentBusDisableSccacheForCargoSteps = $State.DisableSccacheForCargoSteps
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

function Test-AgentBusSccacheTransportFailure {
    param(
        [object[]]$Output
    )

    $joinedOutput = ($Output | ForEach-Object { $_.ToString() }) -join "`n"
    return $joinedOutput -match "sccache: error: failed to execute compile" -or
        $joinedOutput -match "Failed to send data to or receive data from server" -or
        $joinedOutput -match "Failed to read response header" -or
        $joinedOutput -match "Mismatch of client/server versions" -or
        $joinedOutput -match "sccache: error: timed out" -or
        $joinedOutput -match "sccache server not running" -or
        $joinedOutput -match "Failed to bind socket" -or
        $joinedOutput -match "os error (?:10048|10054)"
}

function Test-AgentBusWritableHealth {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Health
    )

    return $Health.ok -eq $true -and $Health.maintenance.write_blocked -ne $true
}

function Disable-AgentBusSccacheForCargoSteps {
    # Explicit uncached semantics suppress both compiler wrapper layers and
    # their Cargo config environment aliases until this build scope is restored.
    foreach ($name in @('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER',
            'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER')) {
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
    $script:AgentBusDisableSccacheForCargoSteps = $true
}

function Restart-AgentBusBuildWithoutSccache {
    # A workstation may have several independent Cargo builds sharing one
    # sccache server. Never stop that server here: doing so severs unrelated
    # compiler clients. The Cargo config override below is sufficient to make
    # this retry independent of the shared daemon.
    Disable-AgentBusSccacheForCargoSteps
    Write-Warning "sccache failed during compilation; leaving the shared daemon untouched and retrying this cargo step once with Cargo rustc-wrapper disabled."
}

function Resolve-AgentBusNativeCargo {
    # Resolve the repository pin through the native rustup application. A
    # PowerShell alias/function must never reinitialize an explicitly uncached step.
    $repoRoot = Split-Path -Parent $PSScriptRoot
    $pin = @(Select-String -LiteralPath (Join-Path $repoRoot 'rust-toolchain.toml') `
        -Pattern '^\s*channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$')
    if ($pin.Count -ne 1) { throw 'Repository Rust toolchain must contain exactly one stable version pin' }
    $toolchain = $pin[0].Matches[0].Groups[1].Value
    $rustup = @(Microsoft.PowerShell.Core\Get-Command -Name rustup -CommandType Application -ErrorAction Stop)[0]
    $cargoPath = (& $rustup.Source which --toolchain $toolchain cargo 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($cargoPath) -or
        -not (Test-Path -LiteralPath $cargoPath -PathType Leaf)) {
        throw 'Could not resolve installed native Cargo for the repository toolchain'
    }
    $cargo = @(Microsoft.PowerShell.Core\Get-Command -Name $cargoPath -CommandType Application -ErrorAction Stop)[0]
    return [pscustomobject]@{ Path = $cargo.Source; Toolchain = $toolchain }
}

function Invoke-AgentBusRawCargo {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Command,
        [string[]]$AdditionalArgs = @(),
        [switch]$DisableSccache
    )

    $cargoArgs = @()
    $snapshot = @{}
    if ($DisableSccache) {
        $nativeCargo = Resolve-AgentBusNativeCargo
        foreach ($name in @('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER',
                'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER', 'RUSTUP_TOOLCHAIN')) {
            $snapshot[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        }
        $cargoArgs += @('--config', 'build.rustc-wrapper=""', '--config', 'build.rustc-workspace-wrapper=""')
    }
    $cargoArgs += $Command
    $cargoArgs += $AdditionalArgs

    try {
        $capturedCargoOutput = @()
        if ($DisableSccache) {
            foreach ($name in @('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER',
                    'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER')) {
                Remove-Item "Env:$name" -ErrorAction SilentlyContinue
            }
            $env:RUSTUP_TOOLCHAIN = $nativeCargo.Toolchain
            $output = & $nativeCargo.Path @cargoArgs 2>&1 | Tee-Object -Variable capturedCargoOutput
        } else {
            $output = & cargo @cargoArgs 2>&1 | Tee-Object -Variable capturedCargoOutput
        }
        $exitCode = $LASTEXITCODE
        if (-not $output -and $capturedCargoOutput) { $output = $capturedCargoOutput }
        if ($output) { $output | ForEach-Object { Write-Host $_ } }
        return [pscustomobject]@{ ExitCode = $exitCode; Output = @($output) }
    }
    finally {
        foreach ($name in $snapshot.Keys) {
            if ($null -eq $snapshot[$name]) {
                Remove-Item "Env:$name" -ErrorAction SilentlyContinue
            } else {
                Set-Item "Env:$name" $snapshot[$name]
            }
        }
    }
}


function Invoke-AgentBusCargo {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [string]$Command,
        [string[]]$AdditionalArgs = @(),
        [string]$WorkDir
    )

    Write-Host "`n==> $Label"
    if ($WorkDir) {
        Push-Location $WorkDir
    }
    $originalCargoRaw = $env:CARGO_RAW
    try {
        $env:CARGO_RAW = "1"
        $result = Invoke-AgentBusRawCargo `
            -Command $Command `
            -AdditionalArgs $AdditionalArgs `
            -DisableSccache:$script:AgentBusDisableSccacheForCargoSteps
        $exitCode = $result.ExitCode
        if ($exitCode -ne 0 -and (Test-AgentBusSccacheTransportFailure -Output $result.Output)) {
            Restart-AgentBusBuildWithoutSccache
            $result = Invoke-AgentBusRawCargo `
                -Command $Command `
                -AdditionalArgs $AdditionalArgs `
                -DisableSccache
            $exitCode = $result.ExitCode
        }
        if ($exitCode -ne 0) {
            throw "Cargo step failed: $Label (exit code $exitCode)"
        }
    }
    finally {
        if ($null -eq $originalCargoRaw -or $originalCargoRaw -eq "") {
            Remove-Item Env:CARGO_RAW -ErrorAction SilentlyContinue
        }
        else {
            $env:CARGO_RAW = $originalCargoRaw
        }
        if ($WorkDir) {
            Pop-Location
        }
    }
}

function Invoke-AgentBusCargoTest {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [string[]]$CargoArgs,
        [string[]]$NextestArgs,
        [switch]$AllowNextest,
        [bool]$UseNextest = $false,
        [string]$WorkDir
    )

    if ($AllowNextest -and $UseNextest) {
        Invoke-AgentBusCargo -Label "$Label (nextest)" -Command "nextest" -AdditionalArgs $NextestArgs -WorkDir $WorkDir
    }
    else {
        Invoke-AgentBusCargo -Label $Label -Command "test" -AdditionalArgs $CargoArgs -WorkDir $WorkDir
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
        [string]$WorkspaceRoot,
        [Parameter(Mandatory = $true)]
        [string]$TargetDir,
        [Parameter(Mandatory = $true)]
        [string]$BinaryName,
        [string]$Profile = "release"
    )

    $candidates = @(
        (Join-Path $TargetDir "$Profile\$BinaryName.exe"),
        (Join-Path $WorkspaceRoot "target\$Profile\$BinaryName.exe")
    )

    return Find-AgentBusBinary -Candidates $candidates
}
