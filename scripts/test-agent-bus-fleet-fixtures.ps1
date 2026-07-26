$ErrorActionPreference = "Stop"

$doctor = Join-Path $PSScriptRoot "test-agent-bus-fleet.ps1"
$manifest = Join-Path (Split-Path -Parent $PSScriptRoot) "config/fleet/agent-bus-fleet-v1.json"

. $doctor -ManifestPath $manifest -SkipLive -Strict | Out-Null

if (-not (Test-BuildRevisionMatch -VersionText "agent-bus 0.5.0 (v0.5.0-20-gfd70c8d)" -Revision "fd70c8d")) {
    throw "Fleet doctor rejected an exact clean build revision."
}
if (Test-BuildRevisionMatch -VersionText "agent-bus 0.5.0 (v0.5.0-20-gfd70c8d-dirty)" -Revision "fd70c8d") {
    throw "Fleet doctor accepted a dirty build revision."
}
if (Test-BuildRevisionMatch -VersionText "agent-bus 0.5.0 (v0.5.0-20-gfd70c8d-extra)" -Revision "fd70c8d") {
    throw "Fleet doctor accepted a non-exact build revision."
}

$expectedLiteral = ConvertTo-PosixShellLiteral -Path "/opt/agent-bus/bin/agent-bus"
if ($expectedLiteral -ne "'/opt/agent-bus/bin/agent-bus'") {
    throw "Fleet doctor did not preserve a safe manifest path."
}
$unsafePathRejected = $false
try {
    ConvertTo-PosixShellLiteral -Path "/opt/agent-bus;echo-injected" | Out-Null
}
catch {
    $unsafePathRejected = $true
}
if (-not $unsafePathRejected) {
    throw "Fleet doctor accepted an unsafe remote manifest path."
}

$doctorText = Get-Content -LiteralPath $doctor -Raw
foreach ($hardCodedPath in @('$HOME/.local/bin/agent-bus', '$HOME/.config/agent-bus/config.json')) {
    if ($doctorText.Contains($hardCodedPath, [System.StringComparison]::Ordinal)) {
        throw "Fleet doctor contains a hard-coded remote deployment path: $hardCodedPath"
    }
}

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "agent-bus-fleet-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
try {
    $duplicatePath = Join-Path $fixtureRoot "duplicate.json"
    @{
        schema_version          = 1
        authority_machine       = "node-a"
        expected_build_revision = "fd70c8d"
        machines                = @(
            @{
                id = "node-a"; connection = "ssh-linux"; ssh_host = "node-a"; os = "linux"
                architecture = "x86_64"; role = "authority"; canonical_repo = "/repo"
                cli_path = "/bin/agent-bus"; config_path = "/config/agent-bus.json"
                auth_source = "hub-env"
                client_server_url = "http://node-a:8400"
            },
            @{
                id = "node-a"; connection = "ssh-linux"; ssh_host = "node-b"; os = "linux"
                architecture = "x86_64"; role = "client"; canonical_repo = "/repo"
                cli_path = "/bin/agent-bus"; config_path = "/config/agent-bus.json"
                auth_source = "client-config"
                client_server_url = "http://node-a:8400"
            }
        )
    } | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $duplicatePath -Encoding utf8

    $rejected = $false
    try {
        & $doctor -ManifestPath $duplicatePath -SkipLive -Strict | Out-Null
    }
    catch {
        $rejected = $true
    }
    if (-not $rejected) {
        throw "Fleet doctor accepted duplicate machine IDs."
    }

    Write-Output "Fleet doctor fixtures passed."
}
finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}
