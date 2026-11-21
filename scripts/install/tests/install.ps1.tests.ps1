# Requires -Version 5.1
# Run with: Invoke-Pester ./scripts/install/tests/install.ps1.tests.ps1
BeforeAll {
    $installerPath = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\install.ps1'))
    # The registry provider's dynamic -Type parameter does not exist on Unix.
    # A command double gives Pester the same interface on every host and cannot
    # write to the real registry even if a mock accidentally stops matching.
    function Set-ItemProperty {
        [CmdletBinding()]
        param($Path, $Name, $Value, [Microsoft.Win32.RegistryValueKind]$Type)
        throw 'unmocked registry write'
    }
}

Describe 'native ck installer' {
    BeforeEach {
        $originalLocalAppData = $env:LOCALAPPDATA
        $originalPath = $env:Path
        $originalArchitecture = $env:PROCESSOR_ARCHITECTURE
        $originalWowArchitecture = $env:PROCESSOR_ARCHITEW6432
        $originalUserProfile = $env:USERPROFILE
        $env:LOCALAPPDATA = Join-Path $TestDrive 'local-app-data'
        $env:Path = 'C:\Windows\System32;C:\Fixture\Other'
        $env:USERPROFILE = Join-Path $TestDrive 'profile-home'
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
            # Process invocation is mocked; never run a file from the fixture.
            [System.IO.File]::WriteAllText((Join-Path $DestinationPath 'ck.exe'), 'not an executable')
        }
        $registryKey = [pscustomobject]@{
            RawPath = 'C:\Existing\Bin'
            PathKind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        }
        $registryKey | Add-Member ScriptMethod GetValue {
            param($Name, $Default, $Options)
            if ($Name -eq 'PATHEXT') { return '.COM;.EXE;.BAT;.CMD' }
            if ($Options -eq [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames -or
                $this.PathKind -eq [Microsoft.Win32.RegistryValueKind]::String) {
                return $this.RawPath
            }
            return [Environment]::ExpandEnvironmentVariables($this.RawPath)
        }
        $registryKey | Add-Member ScriptMethod GetValueKind { param($Name) return $this.PathKind }
        $registryKey | Add-Member ScriptMethod Dispose {}
        $machineKey = [pscustomobject]@{ RawPath = $null }
        $machineKey | Add-Member ScriptMethod GetValue { param($Name, $Default, $Options) if ($Name -eq 'Path') { return $this.RawPath } }
        $machineKey | Add-Member ScriptMethod GetValueKind { param($Name) return [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $machineKey | Add-Member ScriptMethod Dispose {}
        Mock Get-Item { $registryKey } -ParameterFilter { $LiteralPath -eq 'HKCU:\Environment' }
        Mock Get-Item { $machineKey } -ParameterFilter { $LiteralPath -like 'HKLM:*' }
        $registryKey | Add-Member ScriptMethod GetValueNames { return @('Path') }
        Mock Start-Process {
            param($FilePath, $ArgumentList)
            $destination = Join-Path $env:LOCALAPPDATA 'cortexkit\cmd\ck.exe'
            if ($ArgumentList[0] -eq '__install-bootstrap') {
                $ArgumentList[1] | Should -Be $archiveDigest
                $ArgumentList[2] | Should -Be '0.16.0'
                New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
                Copy-Item -LiteralPath $FilePath -Destination $destination -Force
            }
            else {
                $FilePath | Should -Be $destination
                $ArgumentList[0] | Should -Be 'setup'
                $env:CK_BOOTSTRAP_WINDOWS_PATH | Should -Be '1'
                $registryKey.RawPath = "$(Split-Path -Parent $destination);$($registryKey.RawPath)"
            }
            [pscustomobject]@{ ExitCode = 0 }
        }
        Mock Set-ItemProperty { throw 'bootstrap must not write to the registry' }
    }

    AfterEach {
        $env:LOCALAPPDATA = $originalLocalAppData
        if ($null -eq $originalPath) {
            Remove-Item Env:Path -ErrorAction SilentlyContinue
        }
        else {
            $env:Path = $originalPath
        }
        $env:PROCESSOR_ARCHITECTURE = $originalArchitecture
        if ($null -eq $originalUserProfile) {
            Remove-Item Env:USERPROFILE -ErrorAction SilentlyContinue
        }
        else {
            $env:USERPROFILE = $originalUserProfile
        }
        if ($null -eq $originalWowArchitecture) {
            Remove-Item Env:PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue
        }
        else {
            $env:PROCESSOR_ARCHITEW6432 = $originalWowArchitecture
        }
    }

    It 'gives the extracted helper the archive digest and release version and runs installed setup' {
        $env:CK_RELEASE_INDEX_URL = 'https://release.fixture.example/releases/v1/index.json'
        & $installerPath | Out-Null
        Should -Invoke Invoke-WebRequest -Times 1 -ParameterFilter { $Uri -like '*ck-windows-x64.zip' }
        Should -Invoke Start-Process -Times 1 -ParameterFilter {
            $ArgumentList[0] -eq '__install-bootstrap' -and $ArgumentList[1] -eq $archiveDigest -and $ArgumentList[2] -eq '0.16.0'
        }
        Should -Invoke Start-Process -Times 1 -ParameterFilter { $ArgumentList[0] -eq 'setup' }
        Test-Path (Join-Path $env:LOCALAPPDATA 'cortexkit\installer-manifest.bootstrap.json') | Should -BeFalse
        Should -Invoke Set-ItemProperty -Times 0
    }

    It 'selects the native arm64 archive' {
        $env:PROCESSOR_ARCHITECTURE = 'ARM64'
        & $installerPath | Out-Null
        Should -Invoke Invoke-WebRequest -Times 1 -ParameterFilter { $Uri -like '*ck-windows-arm64.zip' }
    }

    It 'refuses a non-string PATH before invoking the helper' {
        $registryKey.PathKind = [Microsoft.Win32.RegistryValueKind]::DWord
        & $installerPath 2>&1 | Out-Null
        Should -Invoke Start-Process -Times 0
    }

    It 'names a machine PATH shadow without changing it' {
        $machineKey.RawPath = Join-Path $TestDrive 'machine-bin'
        New-Item -ItemType Directory -Path $machineKey.RawPath -Force | Out-Null
        $shadow = Join-Path $machineKey.RawPath 'ck.cmd'
        [IO.File]::WriteAllText($shadow, 'machine CLI')
        $sessionPath = $env:Path
        $originalError = [Console]::Error
        $capturedError = [IO.StringWriter]::new()
        try {
            [Console]::SetError($capturedError)
            & $installerPath | Out-Null
        }
        finally {
            [Console]::SetError($originalError)
        }
        $message = $capturedError.ToString()
        $capturedError.Dispose()
        $message | Should -Match 'cli-shadowed'
        $message | Should -Match ([regex]::Escape($shadow))
        $message | Should -Match ([regex]::Escape("PATH entry $($machineKey.RawPath)"))
        [IO.File]::ReadAllText($shadow) | Should -BeExactly 'machine CLI'
        $env:Path | Should -BeExactly $sessionPath
        Should -Invoke Set-ItemProperty -Times 0
    }

    It 'does not run setup if placement refuses' {
        Mock Start-Process { [pscustomobject]@{ ExitCode = 1 } }
        & $installerPath 2>&1 | Out-Null
        Should -Invoke Start-Process -Times 1 -ParameterFilter { $ArgumentList[0] -eq '__install-bootstrap' }
        Should -Invoke Start-Process -Times 0 -ParameterFilter { $ArgumentList[0] -eq 'setup' }
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
        Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA 'cortexkit\cmd\ck.exe') | Should -BeFalse
        $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    }

}
