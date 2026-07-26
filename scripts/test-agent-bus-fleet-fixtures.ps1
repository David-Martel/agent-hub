$ErrorActionPreference = "Stop"

$doctor = Join-Path $PSScriptRoot "test-agent-bus-fleet.ps1"
$manifest = Join-Path (Split-Path -Parent $PSScriptRoot) "config/fleet/agent-bus-fleet-v1.json"

& $doctor -ManifestPath $manifest -SkipLive -Strict | Out-Null

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
                client_server_url = "http://node-a:8400"
            },
            @{
                id = "node-a"; connection = "ssh-linux"; ssh_host = "node-b"; os = "linux"
                architecture = "x86_64"; role = "client"; canonical_repo = "/repo"
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
