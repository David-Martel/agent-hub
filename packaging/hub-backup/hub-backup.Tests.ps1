# Protects: a dated backup folder is always complete and verified. Detects a manifest that hashes itself, a corrupt
# Redis snapshot accepted, and retention deleting a run after a failure. Fake tools only; no database is touched.
BeforeAll {
    $script:Script = Join-Path $PSScriptRoot 'hub-backup.ps1'

    function New-FakeTool([string]$Bin, [string]$Name, [string]$OutputFlag) {
        $body = @(
            '@echo off'
            ':loop'
            'if "%~1"=="" exit /b 0'
            "if `"%~1`"==`"$OutputFlag`" ( >`"%~2`" echo payload-$Name& exit /b 0 )"
            'shift'
            'goto loop'
        ) -join "`r`n"
        Set-Content (Join-Path $Bin "$Name.cmd") $body -Encoding ascii
    }

    function New-FakeTools([string]$Root, [int]$CheckExit = 0) {
        $bin = Join-Path $Root 'bin'
        New-Item -ItemType Directory -Force $bin | Out-Null
        New-FakeTool $bin 'pg_dump' '-f'
        New-FakeTool $bin 'redis-cli' '--rdb'
        Set-Content (Join-Path $bin 'pg_restore.cmd') "@echo off`r`nexit /b 0" -Encoding ascii
        Set-Content (Join-Path $bin 'redis-check-rdb.cmd') "@echo off`r`nexit /b $CheckExit" -Encoding ascii
        $bin
    }
}

Describe 'hub-backup' {
    BeforeEach {
        $script:Root = Join-Path $TestDrive ([guid]::NewGuid())
        $script:Dest = Join-Path $script:Root 'dest'
        New-Item -ItemType Directory -Force $script:Dest | Out-Null
    }

    It 'finishes a dated folder whose manifest lists only the two closed files, with correct hashes' {
        $bin = New-FakeTools $script:Root
        & $script:Script -Dest $script:Dest -PgBin $bin -RedisBin $bin
        $run = @(Get-ChildItem $script:Dest -Directory)
        $run.Count | Should -Be 1
        $run[0].Name | Should -Match '^\d{8}-\d{6}$'
        $lines = @(Get-Content (Join-Path $run[0].FullName 'SHA256SUMS.txt'))
        $lines.Count | Should -Be 2
        foreach ($line in $lines) {
            $name, $length, $hash = $line -split ' '
            $file = Join-Path $run[0].FullName $name
            (Get-Item $file).Length | Should -Be ([long]$length)
            (Get-FileHash $file -Algorithm SHA256).Hash | Should -Be $hash
        }
        (Get-Content (Join-Path $script:Dest 'backup.log') -Raw) | Should -Match ' ok '
    }

    It 'leaves no dated folder when the snapshot check fails, and records the failure' {
        $bin = New-FakeTools $script:Root -CheckExit 1
        { & $script:Script -Dest $script:Dest -PgBin $bin -RedisBin $bin } | Should -Throw '*redis-check-rdb*'
        @(Get-ChildItem $script:Dest -Directory | Where-Object Name -NotMatch '^\.partial').Count | Should -Be 0
        @(Get-ChildItem $script:Dest -Directory | Where-Object Name -Match '^\.partial').Count | Should -Be 1
        (Get-Content (Join-Path $script:Dest 'backup.log') -Raw) | Should -Match 'FAILED'
    }

    It 'refuses to publish when the bytes on disk differ from what was written' {
        $bin = New-FakeTools $script:Root
        { & $script:Script -Dest $script:Dest -PgBin $bin -RedisBin $bin -DiskHashCommand { param($p) 'DEADBEEF' } } | Should -Throw '*disk copy differs*'
        @(Get-ChildItem $script:Dest -Directory | Where-Object Name -NotMatch '^\.partial').Count | Should -Be 0
        (Get-Content (Join-Path $script:Dest 'backup.log') -Raw) | Should -Match 'FAILED'
    }

    It 'reads real files through the aligned reader, including sizes that are not a sector multiple' {
        $bin = New-FakeTools $script:Root
        & $script:Script -Dest $script:Dest -PgBin $bin -RedisBin $bin
        foreach ($size in 1, 511, 4097, 1048576, 3145729) {
            $file = Join-Path $script:Root "size-$size.bin"
            $bytes = New-Object byte[] $size
            (New-Object Random($size)).NextBytes($bytes)
            [IO.File]::WriteAllBytes($file, $bytes)
            [HubBackup.DiskHash]::Compute($file) | Should -Be (Get-FileHash $file -Algorithm SHA256).Hash
        }
    }

    It 'keeps old runs when a backup fails, and keeps the newest few when one succeeds' {
        foreach ($name in '20200101-000000', '20200102-000000', '20200103-000000', '20200104-000000') {
            $old = New-Item -ItemType Directory (Join-Path $script:Dest $name)
            $old.CreationTime = (Get-Date).AddDays(-60)
        }
        $bad = New-FakeTools (Join-Path $script:Root 'bad') -CheckExit 1
        { & $script:Script -Dest $script:Dest -PgBin $bad -RedisBin $bad } | Should -Throw
        @(Get-ChildItem $script:Dest -Directory | Where-Object Name -Match '^2020').Count | Should -Be 4
        # Folder names have one-second resolution, and the script refuses to reuse one.
        Start-Sleep -Milliseconds 1100
        $good = New-FakeTools (Join-Path $script:Root 'good')
        & $script:Script -Dest $script:Dest -PgBin $good -RedisBin $good
        @(Get-ChildItem $script:Dest -Directory | Where-Object Name -Match '^2020' | Sort-Object Name -Descending | ForEach-Object Name) | Should -Be @('20200104-000000', '20200103-000000')
    }
}
