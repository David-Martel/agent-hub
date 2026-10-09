#Requires -Version 7.4
# Cross-target setup deliberately does not call the permissive native Linux cache bootstrap.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'cache-preflight.ps1')

function Invoke-CrossSetupTool {
    param([string]$Executable, [string[]]$Arguments, [int]$TimeoutSeconds = 30)
    $process = [Diagnostics.Process]::new()
    $process.StartInfo.FileName = $Executable
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $process.StartInfo.ArgumentList.Add($argument) }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $started = $false
    $stdout = $stderr = $null
    $born = 'NOT_ESTABLISHED'
    try {
        if (-not $process.Start()) { throw 'Cross setup client did not start' }
        $started = $true
        try { $born = $process.StartTime.ToUniversalTime().ToString('o') } catch {
            if (-not $process.HasExited) { throw }
        }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $null = $process.WaitForExit([int][Math]::Max(0, $TimeoutSeconds * 1000 - $clock.ElapsedMilliseconds))
        $deadline = -not $process.HasExited -or $clock.ElapsedMilliseconds -ge $TimeoutSeconds * 1000
        if ($deadline -and -not $process.HasExited) { $process.Kill($false) }
        $settlement = [Diagnostics.Stopwatch]::StartNew()
        if (-not $process.HasExited) { $null = $process.WaitForExit(2000) }
        $pipes = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@($stdout, $stderr))
        $null = $pipes.Wait([int][Math]::Max(0, 2000 - $settlement.ElapsedMilliseconds))
        $deadline = $deadline -or $clock.ElapsedMilliseconds -ge $TimeoutSeconds * 1000
        if ($deadline -or -not $process.HasExited -or -not $stdout.IsCompletedSuccessfully -or
            -not $stderr.IsCompletedSuccessfully -or $process.ExitCode -ne 0) {
            throw "Cross setup client failed or did not settle: $([IO.Path]::GetFileName($Executable)) pid=$($process.Id) born=$born elapsed_ms=$($clock.ElapsedMilliseconds)"
        }
        return $stdout.GetAwaiter().GetResult().Trim()
    } finally {
        if ($started -and (-not $process.HasExited -or
                ($stdout -and -not $stdout.IsCompleted) -or ($stderr -and -not $stderr.IsCompleted))) {
            $script:AgentBusUnsettledClients.Add([pscustomobject]@{
                Process = $process; ClientPid = $process.Id; BornUtc = $born
                Executable = $Executable; Stdout = $stdout; Stderr = $stderr
                ElapsedMilliseconds = $clock.ElapsedMilliseconds
            })
        } else { $process.Dispose() }
    }
}

function Assert-CrossRunnerBinding {
    param($Container, [string]$Candidate, [string]$Image, [string]$Revision,
        [string]$DockerfileHash, $Labels, $LocalNamespace, $RemoteNamespace)
    if ($Candidate -cnotmatch '^[a-f0-9]{12,64}$' -or $Container.id -cnotmatch '^[a-f0-9]{64}$' -or
        -not $Container.id.StartsWith($Candidate, [StringComparison]::Ordinal) -or
        $Container.running -isnot [bool] -or -not $Container.running -or $Container.image -cne $Image -or
        $Labels.revision -cne $Revision -or $Labels.dockerfile -cne $DockerfileHash) {
        throw 'Running container differs from the qualified immutable source/image binding'
    }
    foreach ($field in @('boot', 'net', 'pid')) {
        $pattern = switch ($field) {
            boot { '^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$' }
            net { '^net:\[[0-9]+\]$' }
            pid { '^pid:\[[0-9]+\]$' }
        }
        if ($LocalNamespace.$field -cnotmatch $pattern -or
            $LocalNamespace.$field -cne $RemoteNamespace.$field) {
            throw 'Selected Docker container is not this runner kernel/network/PID namespace'
        }
    }
}

function Assert-CrossCacheMarker {
    param($Actual, $Expected)
    foreach ($field in @('schema_version', 'owner', 'uid', 'version', 'image', 'revision', 'dockerfile',
            'target', 'directory', 'config', 'config_sha256', 'executable_sha256')) {
        if ($Actual.$field -cne $Expected.$field) { throw 'Refuse foreign or changed dedicated cache ownership' }
    }
}

function Assert-CrossCacheDaemon {
    param($Binding, $Expected)
    if ($Binding.pid -le 0 -or $Binding.start_ticks -cnotmatch '^[0-9]+$' -or $Binding.uid -cne $Expected.uid -or
        $Binding.executable_sha256 -cne $Expected.executable_sha256 -or
        $Binding.directory -cne $Expected.directory -or $Binding.config -cne $Expected.config -or
        $Binding.port -cne '4228') {
        throw 'Dedicated cache daemon disk/config/executable identity is not established'
    }
}

function Get-CrossCacheDaemon {
    # Inspect only a loopback listener in this already-qualified namespace. Never
    # print raw /proc environment, arguments, or foreign process information.
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $uid = [regex]::Match([IO.File]::ReadAllText('/proc/self/status'), '(?m)^Uid:\s+(\d+)').Groups[1].Value
    $listeners = @()
    foreach ($table in @('/proc/net/tcp', '/proc/net/tcp6')) {
      foreach ($line in [IO.File]::ReadAllLines($table)) {
        $fields = $line.Trim() -split '\s+'
        if ($fields.Count -gt 9 -and $fields[1] -cmatch ':1084$' -and $fields[3] -ceq '0A') {
            if ($table -cne '/proc/net/tcp' -or $fields[1] -cne '0100007F:1084') { throw 'Dedicated cache port has an unqualified address' }
            if ($fields[7] -cne $uid) { throw 'Dedicated cache port belongs to another UID' }
            $listeners += $fields[9]
        }
      }
    }
    if ($listeners.Count -eq 0) { return $null }
    if ($listeners.Count -ne 1) { throw 'Dedicated cache listener is not unique' }
    foreach ($entry in Get-ChildItem -LiteralPath '/proc' -Directory) {
        if ($clock.ElapsedMilliseconds -ge 2000) { throw 'Dedicated cache identity census exceeded its bound' }
        if ($entry.Name -cnotmatch '^[0-9]+$') { continue }
        try {
            $status = [IO.File]::ReadAllText((Join-Path $entry.FullName 'status'))
            if ([regex]::Match($status, '(?m)^Uid:\s+(\d+)').Groups[1].Value -cne $uid) { continue }
            $exe = [IO.FileInfo]::new((Join-Path $entry.FullName 'exe')).LinkTarget
            if ($exe -notmatch '/sccache(?: \(deleted\))?$') { continue }
            $socketMatch = $false
            foreach ($fd in Get-ChildItem -LiteralPath (Join-Path $entry.FullName 'fd')) {
                if ($clock.ElapsedMilliseconds -ge 2000) { throw 'Dedicated cache identity census exceeded its bound' }
                if ($fd.LinkTarget -ceq "socket:[$($listeners[0])]") { $socketMatch = $true; break }
            }
            if (-not $socketMatch) { continue }
            $statPath = Join-Path $entry.FullName 'stat'
            $before = [IO.File]::ReadAllText($statPath)
            $birth = ($before.Substring($before.LastIndexOf(')') + 2) -split '\s+')[19]
            $values = @{}
            foreach ($value in ([Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes((Join-Path $entry.FullName 'environ'))) -split "`0")) {
                $parts = $value -split '=', 2
                if ($parts.Count -eq 2 -and $parts[0] -cin @('SCCACHE_DIR', 'SCCACHE_CONF', 'SCCACHE_SERVER_PORT')) { $values[$parts[0]] = $parts[1] }
            }
            $hash = (Get-FileHash -LiteralPath (Join-Path $entry.FullName 'exe')).Hash.ToLowerInvariant()
            $after = [IO.File]::ReadAllText($statPath)
            if (($after.Substring($after.LastIndexOf(')') + 2) -split '\s+')[19] -cne $birth) { throw 'Cache daemon birth identity changed' }
            return [pscustomobject]@{ pid = [int]$entry.Name; start_ticks = $birth; uid = $uid
                executable_sha256 = $hash; directory = $values['SCCACHE_DIR']; config = $values['SCCACHE_CONF']; port = $values['SCCACHE_SERVER_PORT'] }
        } catch [IO.FileNotFoundException] { continue } catch [IO.DirectoryNotFoundException] { continue }
    }
    throw 'Dedicated cache port is open without an attributable daemon'
}

function Assert-CrossPlainPath {
    param([string]$Path)
    if (-not $Path.StartsWith('/', [StringComparison]::Ordinal) -or $Path -match '[\r\n\x00]') { throw 'Absolute plain Linux cache path required' }
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            if ((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Cache symlink path refused' }
        }
        $next = [IO.Path]::GetDirectoryName($cursor)
        if ($next -eq $cursor) { break }; $cursor = $next
    }
}

function Write-CrossOwnedText {
    param([string]$Path, [string]$Text)
    $options = [IO.FileStreamOptions]::new()
    $options.Mode = [IO.FileMode]::CreateNew
    $options.Access = [IO.FileAccess]::Write
    $options.Share = [IO.FileShare]::Read
    $options.UnixCreateMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite
    $stream = [IO.FileStream]::new($Path, $options)
    try {
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Text)
        $stream.Write($bytes, 0, $bytes.Length)
    } finally { $stream.Dispose() }
}

function Assert-CrossCachePathOwnership {
    param([string]$Path, [string]$Mode)
    Assert-CrossPlainPath $Path
    $uid = [regex]::Match([IO.File]::ReadAllText('/proc/self/status'), '(?m)^Uid:\s+(\d+)').Groups[1].Value
    $stat = (Get-Command stat -CommandType Application -TotalCount 1 -ErrorAction Stop).Source
    if ((Invoke-CrossSetupTool $stat @('-c', '%u:%a', '--', $Path)) -cne "${uid}:$Mode") {
        throw 'Dedicated cache path owner/mode is not private'
    }
}

if (-not $IsLinux -or [Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne 'X64') {
    throw 'Windows cross setup requires Linux X64'
}
foreach ($name in @('AGENT_HUB_WINDOWS_CROSS_IMAGE_ID',
        'AGENT_HUB_WINDOWS_CROSS_IMAGE_SOURCE_REVISION', 'AGENT_HUB_WINDOWS_CROSS_IMAGE_DOCKERFILE_SHA256')) {
    if (-not [Environment]::GetEnvironmentVariable($name)) { throw "Missing qualified runner binding: $name" }
}
$candidate = if ($env:AGENT_HUB_WINDOWS_CROSS_RUNNER_CONTAINER_ID) {
    $env:AGENT_HUB_WINDOWS_CROSS_RUNNER_CONTAINER_ID
} else { [IO.File]::ReadAllText('/etc/hostname').Trim() }
$imageId = $env:AGENT_HUB_WINDOWS_CROSS_IMAGE_ID
$imageRevision = $env:AGENT_HUB_WINDOWS_CROSS_IMAGE_SOURCE_REVISION
$dockerfileHash = $env:AGENT_HUB_WINDOWS_CROSS_IMAGE_DOCKERFILE_SHA256
if ($candidate -cnotmatch '^[a-f0-9]{12,64}$' -or $imageId -cnotmatch '^sha256:[a-f0-9]{64}$' -or
    $imageRevision -cnotmatch '^[a-f0-9]{40}$' -or $dockerfileHash -cnotmatch '^[a-f0-9]{64}$') {
    throw 'Invalid qualified runner binding'
}
$docker = (Get-Command docker -CommandType Application -TotalCount 1 -ErrorAction Stop).Source
$container = Invoke-CrossSetupTool $docker @('inspect', '--format', '{"id":{{json .Id}},"running":{{json .State.Running}},"image":{{json .Image}}}', $candidate) | ConvertFrom-Json
$containerId = $container.id
if ($containerId -cnotmatch '^[a-f0-9]{64}$') { throw 'Docker did not resolve a full runner ID' }
$labels = Invoke-CrossSetupTool $docker @('image', 'inspect', '--format', '{"revision":{{json (index .Config.Labels "org.opencontainers.image.revision")}},"dockerfile":{{json (index .Config.Labels "com.dtm.source.dockerfile-sha256")}}}', $imageId) | ConvertFrom-Json
$localNamespace = [pscustomobject]@{
    boot = [IO.File]::ReadAllText('/proc/sys/kernel/random/boot_id').Trim()
    net = [IO.FileInfo]::new('/proc/self/ns/net').LinkTarget
    pid = [IO.FileInfo]::new('/proc/self/ns/pid').LinkTarget
}
$remoteNamespace = [pscustomobject]@{
    boot = Invoke-CrossSetupTool $docker @('exec', $containerId, 'cat', '/proc/sys/kernel/random/boot_id')
    net = Invoke-CrossSetupTool $docker @('exec', $containerId, 'readlink', '/proc/self/ns/net')
    pid = Invoke-CrossSetupTool $docker @('exec', $containerId, 'readlink', '/proc/self/ns/pid')
}
Assert-CrossRunnerBinding $container $candidate $imageId $imageRevision $dockerfileHash $labels $localNamespace $remoteNamespace
$env:AGENT_HUB_WINDOWS_CROSS_RUNNER_CONTAINER_ID = $containerId
$env:AGENT_BUS_CI_RUNNER_CONTAINER_ID = $containerId

$target = 'x86_64-pc-windows-gnu'
$policy = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'windows-cache-policy.json') -Raw | ConvertFrom-Json
$rustup = (Get-Command rustup -CommandType Application -TotalCount 1 -ErrorAction Stop).Source
$rustc = Invoke-CrossSetupTool $rustup @('which', 'rustc')
$cargo = Invoke-CrossSetupTool $rustup @('which', 'cargo')
$compilerVersion = Invoke-CrossSetupTool $rustc @('--version')
$compilerMetadata = Invoke-CrossSetupTool $rustc @('-vV')
$pin = [regex]::Match([IO.File]::ReadAllText('rust-toolchain.toml'), '(?m)^channel\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $pin -or -not $compilerVersion.StartsWith("rustc $pin ", [StringComparison]::Ordinal)) {
    throw 'Compiler differs from repository toolchain pin'
}
$installedTargets = Invoke-CrossSetupTool $rustup @('target', 'list', '--installed')
if ($target -cnotin ($installedTargets -split '\r?\n')) { throw 'Qualified image lacks Windows GNU standard library' }
$linker = (Get-Command x86_64-w64-mingw32-gcc-posix -CommandType Application -TotalCount 1 -ErrorAction Stop).Source
$linkerVersion = Invoke-CrossSetupTool $linker @('--version')

if (-not $env:RUNNER_TEMP -or -not $env:GITHUB_JOB) { throw 'Private runner job paths are required' }
$jobRoot = Join-Path $env:RUNNER_TEMP ('agent-hub-windows-cross-' + $env:GITHUB_JOB)
if (Test-Path -LiteralPath $jobRoot) { throw 'Refuse pre-existing cross setup directory' }
New-Item -ItemType Directory -Path $jobRoot | Out-Null
[IO.File]::SetUnixFileMode($jobRoot, [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute)
$archive = Join-Path $jobRoot 'sccache.tar.gz'
$archiveHash = '45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89'
$curl = (Get-Command curl -CommandType Application -TotalCount 1 -ErrorAction Stop).Source
$null = Invoke-CrossSetupTool $curl @('--fail', '--location', '--proto', '=https', '--tlsv1.2',
    '--connect-timeout', '10', '--max-time', '60', '--output', $archive,
    'https://github.com/mozilla/sccache/releases/download/v0.18.0/sccache-v0.18.0-x86_64-unknown-linux-musl.tar.gz') 65
if ((Get-FileHash -LiteralPath $archive).Hash.ToLowerInvariant() -cne $archiveHash) { throw 'Pinned sccache archive digest differs' }
$tar = (Get-Command tar -CommandType Application -TotalCount 1 -ErrorAction Stop).Source
$null = Invoke-CrossSetupTool $tar @('-xzf', $archive, '-C', $jobRoot, '--', 'sccache-v0.18.0-x86_64-unknown-linux-musl/sccache')
$sccache = Join-Path $jobRoot 'sccache-v0.18.0-x86_64-unknown-linux-musl/sccache'
if ((Invoke-CrossSetupTool $sccache @('--version')) -cne "sccache $($policy.version)") { throw 'Cross cache version differs from policy' }

# Stable across jobs/recreated containers of the same immutable image. Retain
# older image namespaces; never adopt, stop, reset or replace their cache.
$cacheParent = Join-Path $HOME '.cache/agent-hub/windows-gnu0.18'
Assert-CrossPlainPath $cacheParent
if (-not (Test-Path -LiteralPath $cacheParent)) {
    [IO.Directory]::CreateDirectory($cacheParent, [IO.UnixFileMode]::UserRead -bor
        [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute) | Out-Null
}
Assert-CrossCachePathOwnership $cacheParent '700'
$cacheRoot = Join-Path $cacheParent $imageId.Substring(7)
Assert-CrossPlainPath $cacheRoot
$cacheData = Join-Path $cacheRoot 'data'
$cacheConfig = Join-Path $cacheRoot 'sccache-config.toml'
$cacheMarker = Join-Path $cacheRoot 'owner.json'
$owner = [ordered]@{
    schema_version = 1; owner = 'agent-hub-windows-gnu-cache-v1'; version = '0.18.0'
    uid = [regex]::Match([IO.File]::ReadAllText('/proc/self/status'), '(?m)^Uid:\s+(\d+)').Groups[1].Value
    image = $imageId; revision = $imageRevision; dockerfile = $dockerfileHash; target = $target
    directory = $cacheData; config = $cacheConfig
    config_sha256 = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
    executable_sha256 = (Get-FileHash -LiteralPath $sccache).Hash.ToLowerInvariant()
}
$daemonBefore = Get-CrossCacheDaemon
if (Test-Path -LiteralPath $cacheRoot) {
    foreach ($path in @($cacheMarker, $cacheData, $cacheConfig)) { Assert-CrossPlainPath $path }
    Assert-CrossCacheMarker ([IO.File]::ReadAllText($cacheMarker) | ConvertFrom-Json) $owner
    if ((Get-FileHash -LiteralPath $cacheConfig).Hash.ToLowerInvariant() -cne $owner.config_sha256) { throw 'Dedicated cache config changed' }
} else {
    if ($daemonBefore) { throw 'Refuse first cache ownership while dedicated port is already open' }
    New-Item -ItemType Directory -Path $cacheRoot | Out-Null
    [IO.File]::SetUnixFileMode($cacheRoot, [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute)
    New-Item -ItemType Directory -Path $cacheData | Out-Null
    [IO.File]::SetUnixFileMode($cacheData, [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute)
    Write-CrossOwnedText $cacheConfig ''
    Write-CrossOwnedText $cacheMarker ($owner | ConvertTo-Json -Depth 5)
}
Assert-CrossCachePathOwnership $cacheRoot '700'
Assert-CrossCachePathOwnership $cacheData '700'
Assert-CrossCachePathOwnership $cacheConfig '600'
Assert-CrossCachePathOwnership $cacheMarker '600'
if ($daemonBefore) { Assert-CrossCacheDaemon $daemonBefore $owner }

# All changes are confined to this CI step and its children. Never reset or stop a cache server.
foreach ($name in @([Environment]::GetEnvironmentVariables().Keys)) {
    if ($name -match '^(SCCACHE_|AWS_|AZURE_|GOOGLE_APPLICATION_CREDENTIALS$)' -or $name -eq 'RUSTC_WRAPPER') {
        [Environment]::SetEnvironmentVariable([string]$name, $null, 'Process')
    }
}
$env:CARGO_TARGET_DIR = Join-Path $jobRoot 'target'
$env:CARGO_INCREMENTAL = '0'
$env:SCCACHE_DIR = $cacheData
$env:SCCACHE_SERVER_PORT = [string]$policy.server_port
$env:SCCACHE_CONF = $cacheConfig
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = $linker
# Bind Cargo to the compiler proven above. Explicit empty wrapper values also
# override image/user Cargo config; removing them would restore those defaults.
$env:RUSTC = $rustc
$env:CARGO_BUILD_RUSTC = $rustc
$env:RUSTC_WORKSPACE_WRAPPER = ''
$env:CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER = ''
$env:CARGO_BUILD_RUSTC_WRAPPER = ''
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $HOME '.rustup' }
Assert-CrossPlainPath $cargoHome
Assert-CrossPlainPath $rustupHome
$proof = Invoke-AgentBusCachePreflight -SccachePath $sccache -RustcPath $rustc -TargetDirectory $env:CARGO_TARGET_DIR -Policy $policy -TargetTriple $target
$daemonAfter = Get-CrossCacheDaemon
if (-not $daemonAfter) { throw 'Strict preflight did not establish its dedicated daemon' }
Assert-CrossCacheDaemon $daemonAfter $owner
if ($daemonBefore -and ($daemonAfter.pid -ne $daemonBefore.pid -or $daemonAfter.start_ticks -cne $daemonBefore.start_ticks)) { throw 'Dedicated daemon changed during preflight' }
$env:RUSTC_WRAPPER = $sccache
$receipt = [ordered]@{
    schema_version = 1; runner_container_id = $containerId; runner_image_id = $imageId
    runner_image_source_revision = $imageRevision; runner_dockerfile_sha256 = $dockerfileHash
    rustc = $rustc; rustc_verbose_version = $compilerMetadata; rustc_sha256 = (Get-FileHash -LiteralPath $rustc).Hash
    cargo_sha256 = (Get-FileHash -LiteralPath $cargo).Hash
    cargo = $cargo; cargo_home = $cargoHome; rustup_home = $rustupHome
    linker = $linker; linker_version = $linkerVersion; linker_sha256 = (Get-FileHash -LiteralPath $linker).Hash
    sccache_archive_sha256 = $archiveHash; cache_proof = $proof
    runner_namespace = $localNamespace; dedicated_cache_owner = $owner; dedicated_cache_daemon = $daemonAfter
    target = $target; abi = 'gnu'; host_os = 'Linux'; host_arch = 'X64'; native_windows_validated = $false
}
[IO.File]::WriteAllText((Join-Path $env:CARGO_TARGET_DIR 'cross-toolchain-proof.json'), ($receipt | ConvertTo-Json -Depth 20))
if ($env:GITHUB_ENV) {
    foreach ($name in @('CARGO_TARGET_DIR', 'CARGO_INCREMENTAL', 'SCCACHE_DIR', 'SCCACHE_SERVER_PORT',
            'SCCACHE_CONF', 'RUSTC_WRAPPER', 'CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER',
            'RUSTC', 'CARGO_BUILD_RUSTC', 'RUSTC_WORKSPACE_WRAPPER',
            'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WRAPPER',
            'AGENT_HUB_WINDOWS_CROSS_RUNNER_CONTAINER_ID', 'AGENT_BUS_CI_RUNNER_CONTAINER_ID')) {
        "$name=$([Environment]::GetEnvironmentVariable($name))" | Add-Content -LiteralPath $env:GITHUB_ENV -Encoding utf8
    }
}
if ($env:GITHUB_PATH) { (Split-Path -Parent $cargo) | Add-Content -LiteralPath $env:GITHUB_PATH -Encoding utf8 }
Write-Host 'Windows GNU compiler, source/image binding and strict two-request cache proof established'
