# Requires -Version 5.1
# Run with: Invoke-Pester ./scripts/install/tests/install.ps1.tests.ps1
BeforeAll {
    $installerPath = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\install.ps1'))
}

Describe 'native ck installer' {
    BeforeEach {
        $originalLocalAppData = $env:LOCALAPPDATA
        $originalArchitecture = $env:PROCESSOR_ARCHITECTURE
        $originalWowArchitecture = $env:PROCESSOR_ARCHITEW6432
        $env:LOCALAPPDATA = Join-Path $TestDrive 'local-app-data'
        $env:PROCESSOR_ARCHITECTURE = 'AMD64'
        Remove-Item Env:PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue

        $archiveBytes = [System.Text.Encoding]::UTF8.GetBytes('fixture archive')
        $archiveDigest = [System.BitConverter]::ToString(
            [System.Security.Cryptography.SHA256]::Create().ComputeHash($archiveBytes)
        ).Replace('-', '').ToLowerInvariant()
        $setupMarker = Join-Path $TestDrive 'setup-started'

        Mock Invoke-WebRequest {
            param($Uri, $OutFile)
            if ($Uri.ToString().EndsWith('index.json')) {
                $index = @{
                    schema = 1
                    channel = 'alpha'
                    generated_at_ms = 1788425000000
                    components = @{
                        core = @{
                            repository = 'cortexkit/subconscious'
                            release = 'subc-core-v0.16.0'
                            version = '0.16.0'
                            assets = @{
                                'windows-arm64' = @{
                                    ck = @{
                                        url = 'https://release.fixture.example/ck-windows-arm64.zip'
                                        sha256 = $archiveDigest
                                        bytes = 16
                                        reports = '0.16.0'
                                    }
                                }
                                'windows-x64' = @{
                                    ck = @{
                                        url = 'https://release.fixture.example/ck-windows-x64.zip'
                                        sha256 = $archiveDigest
                                        bytes = 16
                                        reports = '0.16.0'
                                    }
                                }
                            }
                        }
                    }
                }
                ($index | ConvertTo-Json -Depth 8) | Set-Content -LiteralPath $OutFile -Encoding UTF8
            }
            else {
                [System.IO.File]::WriteAllBytes($OutFile, $archiveBytes)
            }
        }
        Mock Expand-Archive {
            param($LiteralPath, $DestinationPath)
            New-Item -ItemType Directory -Path $DestinationPath -Force | Out-Null
            # This deliberately invalid .exe would fail immediately if the installer
            # tried to invoke the candidate or destination as part of setup.
            [System.IO.File]::WriteAllText((Join-Path $DestinationPath 'ck.exe'), 'not an executable')
        }
        Mock Get-ItemPropertyValue { 'C:\Existing\Bin' }
        Mock Set-ItemProperty {}
    }

    AfterEach {
        $env:LOCALAPPDATA = $originalLocalAppData
        $env:PROCESSOR_ARCHITECTURE = $originalArchitecture
        if ($null -eq $originalWowArchitecture) {
            Remove-Item Env:PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue
        }
        else {
            $env:PROCESSOR_ARCHITEW6432 = $originalWowArchitecture
        }
    }

    It 'derives, verifies, installs, records, and prints setup without starting it' {
        $env:CK_RELEASE_INDEX_URL = 'https://release.fixture.example/releases/v1/index.json'
        $output = & $installerPath

        $output | Should -Contain 'Next: ck setup'
        Should -Invoke Invoke-WebRequest -Times 1 -ParameterFilter {
            $Uri -eq 'https://release.fixture.example/releases/v1/index.json'
        }
        Should -Invoke Invoke-WebRequest -Times 1 -ParameterFilter {
            $Uri -eq 'https://release.fixture.example/ck-windows-x64.zip'
        }
        Should -Invoke Expand-Archive -Times 1
        Should -Invoke Set-ItemProperty -Times 1 -ParameterFilter {
            $Path -eq 'HKCU:\Environment' -and $Name -eq 'Path'
        }

        $destination = Join-Path $env:LOCALAPPDATA 'cortexkit\bin\ck.exe'
        $manifest = Join-Path $env:LOCALAPPDATA 'cortexkit\installer-manifest.json'
        Test-Path -LiteralPath $destination | Should -BeTrue
        Test-Path -LiteralPath $manifest | Should -BeTrue
        Test-Path -LiteralPath $setupMarker | Should -BeFalse
        # Two digests of two files, asserted by value: the archive the index
        # carried (currency) and the placed binary's bytes (ownership).
        $placement = (Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json).mutations |
            Where-Object { $_.kind -eq 'binary-placement' }
        $binaryDigest = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
        $archiveDigest | Should -Not -Be $binaryDigest
        $placement.archive_sha256 | Should -Be $archiveDigest
        $placement.sha256 | Should -Be $binaryDigest
        # No byte-order mark: ck's JSON reader refuses one, and Windows
        # PowerShell 5.1 writes one under -Encoding UTF8. ConvertFrom-Json
        # tolerates it, so only a byte-level check can see this.
        ([System.IO.File]::ReadAllBytes($manifest))[0] | Should -Be 0x7B
    }

    It 'installs the arm64 archive on Windows on ARM and records the tuple' {
        # The arm before this one installed under the same LOCALAPPDATA; a
        # re-run onto an existing manifest records to the sidecar and leaves
        # the manifest's platform alone, so this arm must start from nothing.
        Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA 'cortexkit') -ErrorAction SilentlyContinue
        $env:PROCESSOR_ARCHITECTURE = 'ARM64'
        $env:CK_RELEASE_INDEX_URL = 'https://release.fixture.example/releases/v1/index.json'
        $output = & $installerPath

        $output | Should -Contain 'Next: ck setup'
        Should -Invoke Invoke-WebRequest -Times 1 -ParameterFilter {
            $Uri -eq 'https://release.fixture.example/ck-windows-arm64.zip'
        }
        $manifest = Join-Path $env:LOCALAPPDATA 'cortexkit\installer-manifest.json'
        (Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json).platform | Should -Be 'windows-arm64'
        $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    }

    It 'refuses an architecture the release does not ship before any fetch' {
        # Windows re-derives PROCESSOR_ARCHITECTURE for every new process from
        # the real machine, so a child cannot be told it is IA64; the arm runs
        # in-process. Refuse writes the console error stream and exits the
        # script, which yields no exception here, so the observable is what
        # did NOT happen: no index fetch, no archive, no placement.
        # Earlier arms in this Describe placed ck.exe under the same
        # LOCALAPPDATA; clear it so the absence below is this arm's own.
        Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA 'cortexkit') -ErrorAction SilentlyContinue
        $env:PROCESSOR_ARCHITECTURE = 'IA64'
        $env:CK_RELEASE_INDEX_URL = 'https://release.fixture.example/releases/v1/index.json'
        & $installerPath 2>&1 | Out-Null
        Should -Invoke Invoke-WebRequest -Times 0
        Should -Invoke Expand-Archive -Times 0
        Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA 'cortexkit\bin\ck.exe') | Should -BeFalse
        $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    }

    It 'reports an identical extracted candidate as a placement no-op' {
        $env:CK_RELEASE_INDEX_URL = 'https://release.fixture.example/releases/v1/index.json'
        & $installerPath | Out-Null

        $output = & $installerPath

        $output | Should -Contain "ck already matches verified download at $(Join-Path $env:LOCALAPPDATA 'cortexkit\bin\ck.exe'); skipping placement."
        $output | Should -Contain 'Next: ck setup'
        Test-Path -LiteralPath $setupMarker | Should -BeFalse
    }

    # A re-run onto an installed machine. Stand in for `ck setup` by
    # extending the manifest with a daemon row the way setup does. The re-run
    # must not touch that file: the daemon row would be lost and `ck upgrade`
    # would report the daemon as not installed. Its rows go to the sidecar
    # that the next ck load adopts.
    It 'leaves an existing manifest untouched and writes its rows to the bootstrap sidecar' {
        $env:CK_RELEASE_INDEX_URL = 'https://release.fixture.example/releases/v1/index.json'
        & $installerPath | Out-Null
        $root = Join-Path $env:LOCALAPPDATA 'cortexkit'
        $manifest = Join-Path $root 'installer-manifest.json'
        $sidecar = Join-Path $root 'installer-manifest.bootstrap.json'
        $daemon = Join-Path $root 'bin\ck-subc.exe'
        $seeded = [ordered]@{
            schema_version = 1
            platform = 'windows-x64'
            mutations = @(
                [ordered]@{ kind = 'binary-placement'; path = (Join-Path $root 'bin\ck.exe'); sha256 = 'stale-binary'; archive_sha256 = 'stale-archive' },
                [ordered]@{ kind = 'binary-placement'; path = $daemon; sha256 = 'daemon-binary'; archive_sha256 = 'daemon-archive' }
            )
        } | ConvertTo-Json -Depth 5
        [System.IO.File]::WriteAllBytes($manifest, [System.Text.UTF8Encoding]::new($false).GetBytes($seeded))
        $before = (Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash

        $output = & $installerPath

        $output | Should -Contain 'Next: ck setup'
        (Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash | Should -Be $before
        Test-Path -LiteralPath $sidecar | Should -BeTrue
        $rows = (Get-Content -LiteralPath $sidecar -Raw | ConvertFrom-Json).mutations
        $placement = $rows | Where-Object { $_.kind -eq 'binary-placement' }
        @($placement).Count | Should -Be 1
        $placement.path | Should -Be (Join-Path $root 'bin\ck.exe')
        $placement.archive_sha256 | Should -Be $archiveDigest
        ($rows | Where-Object { $_.path -eq $daemon }) | Should -BeNullOrEmpty
        ([System.IO.File]::ReadAllBytes($sidecar))[0] | Should -Be 0x7B
    }
}
