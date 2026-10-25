BeforeAll {
    . "$PSScriptRoot/Start-NativeWindowsRunner.ps1"
    $script:ownerIdentity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
}

Describe 'Existing native runner Docker readiness contract' {
    BeforeEach {
        $script:runner = Join-Path $TestDrive 'registered-runner'
        New-Item -ItemType Directory -Path (Join-Path $runner 'bin') -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $runner 'bin/Runner.Listener.exe') -Value 'fixture'
        Set-Content -LiteralPath (Join-Path $runner 'docker.exe') -Value 'fixture'
        Set-Content -LiteralPath (Join-Path $runner '.runner') -Value (
            @{ gitHubUrl = 'https://github.com/David-Martel/agent-hub'; agentId = 27 } |
                ConvertTo-Json
        )
        Mock Get-RunnerHostProcess { @() }
        Mock Invoke-RegisteredRunnerListener { 0 }
        Mock Write-RunnerReadiness {}
        Mock Start-RunnerDesktopApplication {}
    }

    # Protects owner isolation; detects wrong-account startup; needs Windows/Pester.
    # Breadcrumb: Start-NativeWindowsRunner.ps1 owner check; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'refuses another account before issuing any Docker or runner command' {
        Mock Invoke-RunnerProbe { throw 'Must not probe' }
        { Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity 'other-machine/other-user' } | Should -Throw '*owner identity*'
        Should -Invoke Invoke-RunnerProbe -Times 0
        Should -Invoke Invoke-RegisteredRunnerListener -Times 0
    }

    # Protects the registered listener; detects duplicate startup; needs mocked inventory.
    # Breadcrumb: Start-RegisteredNativeRunner process guard; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'preserves an existing listener instead of creating a duplicate' {
        Mock Get-RunnerHostProcess {
            @{ Name = 'Runner.Listener.exe'; ExecutablePath = Join-Path $runner 'bin/Runner.Listener.exe' }
        }
        Mock Invoke-RunnerProbe { throw 'Must not probe' }
        { Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity } | Should -Throw '*already has a listener*'
        Should -Invoke Invoke-RunnerProbe -Times 0
        Should -Invoke Invoke-RegisteredRunnerListener -Times 0
    }

    # Protects running engines; detects silent Windows-to-Linux switching; needs mocked API.
    # Breadcrumb: Start-RegisteredNativeRunner engine check; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'does not start or switch Docker when a different engine is healthy' {
        Mock Invoke-RunnerProbe { @{ Code = 0; Output = 'windows'; ElapsedMilliseconds = 1 } }
        { Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity } | Should -Throw '*different engine*'
        Should -Invoke Invoke-RunnerProbe -Times 1
        Should -Invoke Invoke-RegisteredRunnerListener -Times 0
    }

    # Protects the positive path; detects remote/default-context substitution; needs mocked API.
    # Breadcrumb: explicit localInfoArguments; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'starts only the listener when Linux Docker is already ready' {
        Mock Invoke-RunnerProbe {
            if ($Arguments[0] -ne '--host' -or $Arguments[1] -ne 'npipe:////./pipe/dockerDesktopLinuxEngine') {
                throw 'Ambient or remote Docker context could satisfy readiness'
            }
            @{ Code = 0; Output = 'linux'; ElapsedMilliseconds = 1 }
        }
        Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity | Should -Be 0
        Should -Invoke Invoke-RunnerProbe -Times 1
        Should -Invoke Invoke-RegisteredRunnerListener -Times 1
    }

    # Protects readiness admission; detects failed application start fallthrough; needs launch mock.
    # Breadcrumb: Start-RunnerDesktopApplication; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'fails closed when the supported Desktop startup fails' {
        Mock Invoke-RunnerProbe { @{ Code = 1; Output = ''; ElapsedMilliseconds = 1 } }
        Mock Start-RunnerDesktopApplication { throw 'Supported Desktop start did not succeed' }
        { Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity } | Should -Throw '*Desktop start did not succeed*'
        Should -Invoke Start-RunnerDesktopApplication -Times 1
        Should -Invoke Invoke-RegisteredRunnerListener -Times 0
    }

    # Protects actual API readiness; detects trusting application launch alone; needs staged responses.
    # Breadcrumb: post-start bounded info loop; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'requires a fresh Linux API response after successful Desktop startup' {
        $script:infoCalls = 0
        Mock Invoke-RunnerProbe {
            $script:infoCalls++
            @{ Code = if ($script:infoCalls -gt 1) { 0 } else { 1 }; Output = 'linux'; ElapsedMilliseconds = 1 }
        }
        Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity | Should -Be 0
        Should -Invoke Invoke-RunnerProbe -Times 2 -ParameterFilter { $Arguments -contains 'info' }
        Should -Invoke Start-RunnerDesktopApplication -Times 1
        Should -Invoke Invoke-RegisteredRunnerListener -Times 1
    }

    # Protects process-result fidelity; detects swallowed native exit/output; needs real PowerShell 7.
    # Breadcrumb: Invoke-RunnerProbe; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'settles a bounded real CLI probe without leaking output into its result' {
        $result = Invoke-RunnerProbe -Executable (Get-Process -Id $PID).Path -Arguments @(
            '-NoProfile', '-Command', '[Console]::WriteLine("probe-fixture"); exit 3'
        ) -TimeoutSeconds 10
        $result.Code | Should -Be 3
        $result.Output | Should -Be 'probe-fixture'
    }

    # Protects per-child local Docker binding; detects inherited remote overrides; needs real child.
    # Breadcrumb: Invoke-RunnerProbe Environment.Remove; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'removes ambient Docker connection overrides only from its child' {
        $oldHost = $env:DOCKER_HOST
        $oldContext = $env:DOCKER_CONTEXT
        try {
            $env:DOCKER_HOST = 'tcp://remote-fixture.invalid:2376'
            $env:DOCKER_CONTEXT = 'remote-fixture'
            $result = Invoke-RunnerProbe -Executable (Get-Process -Id $PID).Path -Arguments @(
                '-NoProfile', '-Command', '[Console]::WriteLine(($env:DOCKER_HOST + "|" + $env:DOCKER_CONTEXT)); exit 0'
            ) -TimeoutSeconds 10
            $result.Code | Should -Be 0
            $result.Output | Should -Be '|'
            $env:DOCKER_HOST | Should -Be 'tcp://remote-fixture.invalid:2376'
            $env:DOCKER_CONTEXT | Should -Be 'remote-fixture'
        } finally {
            $env:DOCKER_HOST = $oldHost
            $env:DOCKER_CONTEXT = $oldContext
        }
    }

    # Protects finite client ownership; detects unbounded probe hangs; needs real sleeping child.
    # Breadcrumb: Invoke-RunnerProbe direct-client timeout; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'terminates only its owned finite CLI client on timeout' {
        $result = Invoke-RunnerProbe -Executable (Get-Process -Id $PID).Path -Arguments @(
            '-NoProfile', '-Command', 'Start-Sleep -Seconds 10'
        ) -TimeoutSeconds 1
        $result.Code | Should -Be 124
        $result.TimedOut | Should -BeTrue
        Get-Process -Id $result.ClientPid -ErrorAction SilentlyContinue | Should -BeNullOrEmpty
    }

    # Protects dry-run behavior; detects state changes during WhatIf; needs Pester command mocks.
    # Breadcrumb: ShouldProcess admission; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'does not issue lifecycle commands during WhatIf' {
        Mock Invoke-RunnerProbe { throw 'Must not probe' }
        Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity -WhatIf | Should -Be 0
        Should -Invoke Invoke-RunnerProbe -Times 0
        Should -Invoke Invoke-RegisteredRunnerListener -Times 0
        Should -Invoke Start-RunnerDesktopApplication -Times 0
    }

    # Protects a late owner; detects startup races that duplicate listeners; needs sequenced inventory.
    # Breadcrumb: second listener guard; NATIVE_WINDOWS_RUNNER_TEST_REVIEW.json.
    It 'refuses a listener that appears after Docker readiness' {
        $script:inventoryCalls = 0
        Mock Get-RunnerHostProcess {
            $script:inventoryCalls++
            if ($script:inventoryCalls -gt 1) {
                @{ Name = 'Runner.Listener.exe'; ExecutablePath = Join-Path $runner 'bin/Runner.Listener.exe' }
            }
        }
        Mock Invoke-RunnerProbe { @{ Code = 0; Output = 'linux'; ElapsedMilliseconds = 1 } }
        { Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity } | Should -Throw '*already has a listener*'
        Should -Invoke Invoke-RegisteredRunnerListener -Times 0
    }

    # Protects a concurrently launched Desktop; detects stale process-inventory admission.
    # Needs sequenced inventory and API responses. Breadcrumb: fresh prelaunch inventory guard.
    It 'preserves Desktop appearing during the local API probe' {
        $script:inventoryCalls = 0; $script:infoCalls = 0
        Mock Get-RunnerHostProcess {
            $script:inventoryCalls++
            if ($script:inventoryCalls -gt 1) { @{ Name = 'Docker Desktop.exe'; ExecutablePath = 'fixture' } }
        }
        Mock Invoke-RunnerProbe {
            $script:infoCalls++
            @{ Code = if ($script:infoCalls -gt 1) { 0 } else { 1 }; Output = 'linux'; ElapsedMilliseconds = 1 }
        }
        Start-RegisteredNativeRunner -Directory $runner -Docker (Join-Path $runner 'docker.exe') `
            -Identity $ownerIdentity | Should -Be 0
        Should -Invoke Start-RunnerDesktopApplication -Times 0
        Should -Invoke Invoke-RegisteredRunnerListener -Times 1
    }
}

Describe 'Supported owner application launch' {
    # Protects owner application startup without shell/DDE or another Desktop CLI plugin.
    # Detects a visible/unqualified executable and unbounded helper call. Needs a finite-probe double.
    # Breadcrumb: Start-RunnerDesktopApplication finite helper; real probe timeout selector above.
    It 'launches only the qualified application through a bounded hidden native helper' {
        $application = Join-Path $TestDrive 'Docker Desktop.exe'
        Set-Content -LiteralPath $application -Value 'fixture'
        Mock Invoke-RunnerProbe { @{ Code = 0; Output = '4242' } }
        Start-RunnerDesktopApplication -Executable $application -TimeoutSeconds 3 | Should -Be 4242
        Should -Invoke Invoke-RunnerProbe -Times 1 -ParameterFilter {
            $scriptText = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String($Arguments[-1]))
            $TimeoutSeconds -eq 3 -and $Arguments -contains '-NonInteractive' -and
                $scriptText.Contains('$info.UseShellExecute = $false') -and
                $scriptText.Contains('$info.CreateNoWindow = $true') -and
                $scriptText.Contains($application) -and
                $scriptText.Contains('[Diagnostics.ProcessWindowStyle]::Hidden')
        }
    }

    # Protects finite startup; detects promoting a timed-out launcher into readiness.
    # Needs the same owned-probe boundary. Breadcrumb: launch deadline and real CLI timeout control.
    It 'rejects a timed-out application launch helper' {
        $application = Join-Path $TestDrive 'Docker Desktop.exe'
        Set-Content -LiteralPath $application -Value 'fixture'
        Mock Invoke-RunnerProbe { @{ Code = 124; Output = ''; TimedOut = $true } }
        { Start-RunnerDesktopApplication -Executable $application -TimeoutSeconds 1 } |
            Should -Throw '*launch failed or exceeded its deadline*'
    }
}

Describe 'Limited task process ownership' {
    # Protects an unrelated service runner; detects dependence on administrator-only image paths.
    # Needs mocked CIM service/process data. Breadcrumb: Get-RunnerHostProcess and review register.
    It 'qualifies an unreadable foreign listener by its registered service parent' {
        Mock Get-CimInstance {
            if ($ClassName -eq 'Win32_Process') {
                @{ Name = 'Runner.Listener.exe'; ProcessId = 12232; ParentProcessId = 17616; ExecutablePath = $null }
            } else {
                @{ Name = 'actions.runner.David-Martel-hyperacusis.example'; State = 'Running'; ProcessId = 17616; PathName = '"C:\actions-runner\hyperacusis\bin\RunnerService.exe"' }
            }
        }
        $processes = @(Get-RunnerHostProcess)
        $processes[0].ExecutablePath | Should -BeNullOrEmpty
        $processes[0].ServiceListenerPath | Should -Be 'C:\actions-runner\hyperacusis\bin\Runner.Listener.exe'
        { Assert-RunnerListenerAbsent -Processes $processes -Listener 'C:\actions-runner\agent-hub\bin\Runner.Listener.exe' } | Should -Not -Throw
    }

    # Protects unknown process custody; detects treating an unreadable image as safely absent.
    # Needs mocked CIM inventory with no matching service. Breadcrumb: Assert-RunnerListenerAbsent.
    It 'refuses an unreadable listener without registered service parent proof' {
        Mock Get-CimInstance {
            if ($ClassName -eq 'Win32_Process') {
                @{ Name = 'Runner.Listener.exe'; ProcessId = 12232; ParentProcessId = 77; ExecutablePath = $null }
            }
        }
        { Assert-RunnerListenerAbsent -Processes @(Get-RunnerHostProcess) -Listener 'C:\actions-runner\agent-hub\bin\Runner.Listener.exe' } | Should -Throw '*identity is unreadable*'
    }
}
