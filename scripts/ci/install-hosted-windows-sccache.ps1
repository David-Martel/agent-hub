$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Only an ephemeral hosted runner owns the executable this bootstrap replaces.
if (-not $IsWindows -or $env:RUNNER_ENVIRONMENT -cne 'github-hosted') {
    throw 'Pinned sccache bootstrap requires an ephemeral GitHub-hosted Windows runner'
}
foreach ($required in @('RUNNER_TEMP', 'USERPROFILE', 'GITHUB_ENV')) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($required))) {
        throw "Required hosted runner environment is unavailable: $required"
    }
}
foreach ($compilerOverride in @('RUSTC', 'RUSTC_WORKSPACE_WRAPPER')) {
    if (-not [string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($compilerOverride))) {
        throw "Hosted compiler override is not permitted: $compilerOverride"
    }
}

$version = '0.18.0'
$archiveName = "sccache-v$version-x86_64-pc-windows-msvc"
$archiveUrl = "https://github.com/mozilla/sccache/releases/download/v$version/$archiveName.zip"
$archiveSha256 = '8965c74d5e8a225244f741e18ad2f3f504f48228dc1bac948fc22761a348363d'
$policy = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'windows-cache-policy.json') -Raw | ConvertFrom-Json
if ($policy.version -cne $version) { throw 'Pinned hosted sccache release differs from Windows cache policy' }
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$pin = @(Select-String -LiteralPath (Join-Path $repoRoot 'rust-toolchain.toml') -Pattern '^\s*channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$')
if ($pin.Count -ne 1) { throw 'Repository Rust toolchain must contain exactly one stable version pin' }
$toolchain = $pin[0].Matches[0].Groups[1].Value
# Replace any image override with the repository pin for this and later steps.
Remove-Item Env:RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue
$env:RUSTUP_TOOLCHAIN = $toolchain

$runnerTempRoot = [IO.Path]::GetFullPath($env:RUNNER_TEMP)
$ownedRoot = [IO.Path]::GetFullPath((Join-Path $runnerTempRoot "agent-hub-sccache-$([guid]::NewGuid().ToString('N'))"))
if (-not $ownedRoot.StartsWith($runnerTempRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar,
        [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Owned sccache bootstrap directory is outside runner temporary storage'
}
New-Item -ItemType Directory -Path $ownedRoot | Out-Null
$client = [Net.Http.HttpClient]::new()
$deadline = [Threading.CancellationTokenSource]::new([TimeSpan]::FromSeconds(30))
$response = $null
$download = $null
$versionProcess = $null
try {
    $archivePath = Join-Path $ownedRoot "$archiveName.zip"
    $client.Timeout = [Threading.Timeout]::InfiniteTimeSpan
    $response = $client.GetAsync($archiveUrl, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $deadline.Token).GetAwaiter().GetResult()
    $response.EnsureSuccessStatusCode() | Out-Null
    $download = [IO.File]::Open($archivePath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $response.Content.CopyToAsync($download, $deadline.Token).GetAwaiter().GetResult() | Out-Null
    $download.Dispose()
    $download = $null
    if ((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant() -cne $archiveSha256) {
        throw 'Pinned sccache archive SHA-256 mismatch; refusing extraction'
    }

    $extractedRoot = Join-Path $ownedRoot 'extracted'
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractedRoot
    $expectedExe = Join-Path (Join-Path $extractedRoot $archiveName) 'sccache.exe'
    $executables = @(Get-ChildItem -LiteralPath $extractedRoot -Recurse -File -Filter '*.exe')
    if ($executables.Count -ne 1 -or $executables[0].FullName -cne $expectedExe) {
        throw 'Pinned sccache archive has an unexpected executable layout'
    }
    $versionProcess = [Diagnostics.Process]::new()
    $versionProcess.StartInfo.FileName = $expectedExe
    $versionProcess.StartInfo.ArgumentList.Add('--version')
    $versionProcess.StartInfo.UseShellExecute = $false
    $versionProcess.StartInfo.CreateNoWindow = $true
    $versionProcess.StartInfo.RedirectStandardOutput = $true
    $versionProcess.StartInfo.RedirectStandardError = $true
    if (-not $versionProcess.Start()) { throw 'Could not start verified sccache version command' }
    $versionStdout = $versionProcess.StandardOutput.ReadToEndAsync()
    $versionStderr = $versionProcess.StandardError.ReadToEndAsync()
    if (-not $versionProcess.WaitForExit(30000)) {
        # Own only this exact version client, never a process tree or cache daemon.
        $versionProcess.Kill()
        $versionProcess.WaitForExit(2000) | Out-Null
        throw 'Pinned sccache version command exceeded its 30-second deadline'
    }
    if (-not [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($versionStdout, $versionStderr), 2000)) {
        throw 'Pinned sccache version client exited with output pipes still open'
    }
    $reportedVersion = $versionStdout.GetAwaiter().GetResult().Trim()
    $reportedErrors = $versionStderr.GetAwaiter().GetResult()
    if ($versionProcess.ExitCode -ne 0 -or $reportedVersion -cne "sccache $version" -or
        -not [string]::IsNullOrWhiteSpace($reportedErrors)) {
        throw 'Pinned sccache executable failed exact version verification'
    }
    $executableSha256 = (Get-FileHash -LiteralPath $expectedExe -Algorithm SHA256).Hash.ToLowerInvariant()
    $installDirectory = Join-Path $env:USERPROFILE '.cargo/bin'
    New-Item -ItemType Directory -Force -Path $installDirectory | Out-Null
    $installedExe = Join-Path $installDirectory 'sccache.exe'
    Copy-Item -LiteralPath $expectedExe -Destination $installedExe -Force
    if ((Get-FileHash -LiteralPath $installedExe -Algorithm SHA256).Hash.ToLowerInvariant() -cne $executableSha256) {
        throw 'Installed sccache executable differs from verified release'
    }
    @(
        "RUSTUP_TOOLCHAIN=$toolchain"
        "AGENT_HUB_SCCACHE_ARCHIVE_SHA256=$archiveSha256"
        "AGENT_HUB_SCCACHE_EXECUTABLE_SHA256=$executableSha256"
    ) | Add-Content -LiteralPath $env:GITHUB_ENV -Encoding utf8
    Write-Host "Installed pinned $reportedVersion; archive_sha256=$archiveSha256; executable_sha256=$executableSha256"
}
finally {
    if ($versionProcess) { $versionProcess.Dispose() }
    if ($download) { $download.Dispose() }
    if ($response) { $response.Dispose() }
    $deadline.Dispose()
    $client.Dispose()
    # The random directory was created by this invocation, never discovered by name.
    Remove-Item -LiteralPath $ownedRoot -Recurse -Force
}
