# Requires -Version 5.1
# Run with: Invoke-Pester ./scripts/install/tests/install.ps1.tests.ps1
$script:installer = Join-Path $PSScriptRoot '..\install.ps1'

Describe 'native ck installer' {
    BeforeEach {
        $script:originalLocalAppData = $env:LOCALAPPDATA
        $script:originalArchitecture = $env:PROCESSOR_ARCHITECTURE
        $script:originalWowArchitecture = $env:PROCESSOR_ARCHITEW6432
        $env:LOCALAPPDATA = Join-Path $TestDrive 'local-app-data'
        $env:PROCESSOR_ARCHITECTURE = 'AMD64'
        Remove-Item Env:PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue

        $script:archiveBytes = [System.Text.Encoding]::UTF8.GetBytes('fixture archive')
        $script:archiveDigest = [System.BitConverter]::ToString(
            [System.Security.Cryptography.SHA256]::Create().ComputeHash($script:archiveBytes)
        ).Replace('-', '').ToLowerInvariant()
        $script:setupMarker = Join-Path $TestDrive 'setup-started'

        Mock Invoke-WebRequest {
            param($Uri, $OutFile)
            if ($Uri.EndsWith('.sha256')) {
                [System.IO.File]::WriteAllText($OutFile, "$script:archiveDigest  ck-windows-x64.zip")
            }
            else {
                [System.IO.File]::WriteAllBytes($OutFile, $script:archiveBytes)
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
        $env:LOCALAPPDATA = $script:originalLocalAppData
        $env:PROCESSOR_ARCHITECTURE = $script:originalArchitecture
        if ($null -eq $script:originalWowArchitecture) {
            Remove-Item Env:PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue
        }
        else {
            $env:PROCESSOR_ARCHITEW6432 = $script:originalWowArchitecture
        }
    }

    It 'derives, verifies, installs, records, and prints setup without starting it' {
        $output = & $script:installer -ReleaseBaseUrl 'https://release.fixture.example/download'

        $output | Should -Contain 'Next: ck setup'
        Assert-MockCalled Invoke-WebRequest -Times 1 -ParameterFilter {
            $Uri -eq 'https://release.fixture.example/download/ck-windows-x64.zip'
        }
        Assert-MockCalled Invoke-WebRequest -Times 1 -ParameterFilter {
            $Uri -eq 'https://release.fixture.example/download/ck-windows-x64.zip.sha256'
        }
        Assert-MockCalled Expand-Archive -Times 1
        Assert-MockCalled Set-ItemProperty -Times 1 -ParameterFilter {
            $Path -eq 'HKCU:\Environment' -and $Name -eq 'Path'
        }

        $destination = Join-Path $env:LOCALAPPDATA 'cortexkit\bin\ck.exe'
        $manifest = Join-Path $env:LOCALAPPDATA 'cortexkit\installer-manifest.json'
        Test-Path -LiteralPath $destination | Should -BeTrue
        Test-Path -LiteralPath $manifest | Should -BeTrue
        Test-Path -LiteralPath $script:setupMarker | Should -BeFalse
    }

    It 'reports an identical extracted candidate as a placement no-op' {
        & $script:installer -ReleaseBaseUrl 'https://release.fixture.example/download' | Out-Null

        $output = & $script:installer -ReleaseBaseUrl 'https://release.fixture.example/download'

        $output | Should -Contain "ck already matches verified download at $(Join-Path $env:LOCALAPPDATA 'cortexkit\bin\ck.exe'); skipping placement."
        $output | Should -Contain 'Next: ck setup'
        Test-Path -LiteralPath $script:setupMarker | Should -BeFalse
    }
}
