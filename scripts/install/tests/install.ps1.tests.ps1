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
    }

    It 'reports an identical extracted candidate as a placement no-op' {
        $env:CK_RELEASE_INDEX_URL = 'https://release.fixture.example/releases/v1/index.json'
        & $installerPath | Out-Null

        $output = & $installerPath

        $output | Should -Contain "ck already matches verified download at $(Join-Path $env:LOCALAPPDATA 'cortexkit\bin\ck.exe'); skipping placement."
        $output | Should -Contain 'Next: ck setup'
        Test-Path -LiteralPath $setupMarker | Should -BeFalse
    }
}
