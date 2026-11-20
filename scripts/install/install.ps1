# Verify the downloaded CLI archive, then let ck place and record it. Bootstrap
# trusts TLS to the release host; setup verifies the signed release index.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$IndexUrlDefault = 'https://cortexkit.io/releases/v1/index.json'

function Refuse {
    param(
        [Parameter(Mandatory = $true)][string]$Type,
        [Parameter(Mandatory = $true)][string]$Evidence
    )

    [Console]::Error.WriteLine(("refusal: {0}: {1}" -f $Type, $Evidence))
    exit 1
}

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    try {
        return (Get-FileHash -LiteralPath $Path -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
    }
    catch {
        Refuse 'digest-verification-failed' "Get-FileHash could not hash $Path ($($_.Exception.Message))"
    }
}

function Resolve-CkApplication {
    param([string]$SearchPath, [string]$PathExt)
    foreach ($entry in ($SearchPath -split ';')) {
        $directory = $entry.Trim().Trim('"')
        if ([string]::IsNullOrWhiteSpace($directory)) { continue }
        foreach ($extension in ($PathExt -split ';')) {
            if ([string]::IsNullOrWhiteSpace($extension)) { continue }
            try { $file = Join-Path $directory ("ck" + $extension.ToLowerInvariant()) -ErrorAction Stop }
            catch { continue }
            if (Test-Path -LiteralPath $file -PathType Leaf) { return $file }
        }
    }
    return $null
}

function Get-FreshShellEnvironment {
    $values = @{}
    foreach ($scope in @('Machine', 'User')) {
        $keyPath = 'HKCU:\Environment'
        if ($scope -eq 'Machine') { $keyPath = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment' }
        $key = $null
        try {
            $key = Get-Item -LiteralPath $keyPath -ErrorAction Stop
            foreach ($name in @('Path', 'PATHEXT')) {
                $raw = $key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                $value = [string]$raw
                if ($null -ne $raw -and $key.GetValueKind($name) -eq [Microsoft.Win32.RegistryValueKind]::ExpandString) {
                    $value = [Environment]::ExpandEnvironmentVariables($value)
                }
                $values["$scope$name"] = $value
            }
        }
        catch { Refuse 'path-update-failed' "could not read $keyPath ($($_.Exception.Message))" }
        finally { if ($null -ne $key) { $key.Dispose() } }
    }
    $extensions = $values['UserPATHEXT']
    if ([string]::IsNullOrWhiteSpace($extensions)) { $extensions = $values['MachinePATHEXT'] }
    if ([string]::IsNullOrWhiteSpace($extensions)) { $extensions = '.COM;.EXE;.BAT;.CMD' }
    return @{ Path = ($values['MachinePath'] + ';' + $values['UserPath']); PathExt = $extensions }
}

function Get-FinalCliPath {
    param([string]$Path)
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        $parent = [IO.DirectoryInfo]::new([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($Path)))
        $target = $parent.ResolveLinkTarget($true)
        if ($null -ne $target) { $parent = $target }
        return (Join-Path $parent.FullName ([IO.Path]::GetFileName($Path)))
    }
    if (-not ('CortexKit.CliPath' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
namespace CortexKit {
    public static class CliPath {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern uint GetFinalPathNameByHandle(SafeFileHandle file, StringBuilder path, uint length, uint flags);
        public static string Final(string path) {
            using (var file = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete)) {
                var result = new StringBuilder(32768);
                uint length = GetFinalPathNameByHandle(file.SafeFileHandle, result, (uint)result.Capacity, 0);
                if (length == 0 || length >= result.Capacity) throw new IOException("could not resolve final application path: " + path);
                return result.ToString();
            }
        }
    }
}
'@ -ErrorAction Stop
    }
    return [CortexKit.CliPath]::Final($Path)
}

function Test-SameCliFile {
    param([string]$Left, [string]$Right)
    if ([string]::IsNullOrWhiteSpace($Left) -or -not (Test-Path -LiteralPath $Left -PathType Leaf) -or -not (Test-Path -LiteralPath $Right -PathType Leaf)) { return $false }
    try { return (Get-FinalCliPath $Left) -ieq (Get-FinalCliPath $Right) }
    catch { Refuse 'cli-shadowed' "could not verify application identity for $Left ($($_.Exception.Message))" }
}

function Check-UserPath {
    try {
        $key = Get-Item -LiteralPath 'HKCU:\Environment' -ErrorAction Stop
        try {
            if ($key.GetValueNames() -contains 'Path') {
                $kind = $key.GetValueKind('Path')
                if ($kind -notin @([Microsoft.Win32.RegistryValueKind]::String, [Microsoft.Win32.RegistryValueKind]::ExpandString)) {
                    Refuse 'path-update-failed' 'HKCU Environment Path has an unsupported registry value type'
                }
            }
        }
        finally { $key.Dispose() }
    }
    catch { Refuse 'path-update-failed' "could not read the user PATH registry value ($($_.Exception.Message))" }
}
function Confirm-UserPath {
    param([Parameter(Mandatory = $true)][string]$CmdDir)
    # A new process combines machine PATH before user PATH; the installer's
    # inherited environment cannot prove which application a new shell finds.
    $destination = Join-Path $CmdDir 'ck.exe'
    $fresh = Get-FreshShellEnvironment
    $winner = Resolve-CkApplication -SearchPath $fresh.Path -PathExt $fresh.PathExt
    if (-not (Test-SameCliFile $winner $destination)) {
        $pathEntry = '<none>'
        if (-not [string]::IsNullOrWhiteSpace($winner)) { $pathEntry = Split-Path -Parent $winner }
        Refuse 'cli-shadowed' "ck resolves to $winner (PATH entry $pathEntry) instead of $destination; machine PATH may require an administrator"
    }

    $sessionEntries = @()
    if (-not [string]::IsNullOrWhiteSpace($env:Path)) {
        $sessionEntries = @($env:Path -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    $env:Path = (@($CmdDir) + @($sessionEntries | Where-Object { $_ -ine $CmdDir })) -join ';'
}

function New-PrivateStage {
    param([Parameter(Mandatory = $true)][string]$Path)

    $user = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetOwner($user)
    $security.SetAccessRuleProtection($true, $false)
    $rule = [Security.AccessControl.FileSystemAccessRule]::new($user, 'FullControl', 'ContainerInherit, ObjectInherit', 'None', 'Allow')
    $security.AddAccessRule($rule)
    if (Test-Path -LiteralPath $Path) { Refuse 'extraction-failed' "temporary path already exists: $Path" }
    try {
        if ($PSVersionTable.PSEdition -eq 'Desktop') { [IO.Directory]::CreateDirectory($Path, $security) | Out-Null }
        else { [IO.FileSystemAclExtensions]::CreateDirectory($security, $Path) | Out-Null }
    }
    catch { Refuse 'extraction-failed' "could not create private staging directory: $($_.Exception.Message)" }
}
$IndexUrl = $env:CK_RELEASE_INDEX_URL
if ([string]::IsNullOrWhiteSpace($IndexUrl)) {
    $IndexUrl = $IndexUrlDefault
}

$rawArchitecture = $env:PROCESSOR_ARCHITEW6432
if ([string]::IsNullOrWhiteSpace($rawArchitecture)) {
    $rawArchitecture = $env:PROCESSOR_ARCHITECTURE
}
if ([string]::IsNullOrWhiteSpace($rawArchitecture)) {
    Refuse 'unsupported-platform' 'could not determine Windows architecture'
}
switch ($rawArchitecture.ToLowerInvariant()) {
    'amd64' { $arch = 'x64' }
    'x86_64' { $arch = 'x64' }
    'arm64' { $arch = 'arm64' }
    'aarch64' { $arch = 'arm64' }
    default { $arch = $rawArchitecture.ToLowerInvariant() }
}
if ($arch -ne 'x64' -and $arch -ne 'arm64') {
    Refuse 'unsupported-platform' "windows-$arch; supported tuples are darwin-arm64, linux-x64, linux-arm64, windows-x64, windows-arm64"
}

if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
    Refuse 'placement-failed' 'LOCALAPPDATA is unavailable for the user-scoped installation'
}

$os = 'windows'
$archiveName = "ck-$os-$arch.zip"
$dataDir = Join-Path $env:LOCALAPPDATA 'cortexkit'
$cmdDir = Join-Path $dataDir 'cmd'
$destination = Join-Path $cmdDir 'ck.exe'
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ck-install-" + [System.Guid]::NewGuid().ToString('N'))
$archivePath = Join-Path $tempDir $archiveName
$extractDir = Join-Path $tempDir 'extracted'
$indexPath = Join-Path $tempDir 'index.json'

try {
    New-PrivateStage -Path $tempDir
    try {
        Invoke-WebRequest -Uri $IndexUrl -OutFile $indexPath -UseBasicParsing -ErrorAction Stop
    }
    catch {
        Refuse 'release-incomplete' "release index unavailable: $IndexUrl ($($_.Exception.Message))"
    }
    try {
        $index = Get-Content -LiteralPath $indexPath -Raw -ErrorAction Stop | ConvertFrom-Json
        $asset = $index.components.core.assets."windows-$arch".ck
        $archiveUrl = [string]$asset.url
        $expectedDigest = ([string]$asset.sha256).ToLowerInvariant()
        $releaseVersion = [string]$index.components.core.version
    }
    catch {
        Refuse 'release-incomplete' "ck asset for windows-$arch is missing from $IndexUrl ($($_.Exception.Message))"
    }
    if ([string]::IsNullOrWhiteSpace($archiveUrl) -or $expectedDigest -notmatch '^[a-f0-9]{64}$' -or [string]::IsNullOrWhiteSpace($releaseVersion)) {
        Refuse 'release-incomplete' "ck asset for windows-$arch is missing from $IndexUrl"
    }
    try {
        Invoke-WebRequest -Uri $archiveUrl -OutFile $archivePath -UseBasicParsing -ErrorAction Stop
    }
    catch {
        Refuse 'release-incomplete' "ck archive unavailable: $archiveName from $archiveUrl ($($_.Exception.Message))"
    }
    $actualDigest = Get-Sha256 -Path $archivePath
    if ($actualDigest -ne $expectedDigest) {
        Refuse 'digest-mismatch' "$archiveName expected $expectedDigest but downloaded $actualDigest"
    }

    try {
        Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force -ErrorAction Stop
    }
    catch {
        Refuse 'extraction-failed' "could not extract $archiveName ($($_.Exception.Message))"
    }
    $candidate = Join-Path $extractDir 'ck.exe'
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        Refuse 'extraction-failed' "$archiveName did not contain ck.exe at its archive root"
    }
    Check-UserPath
    $placement = Start-Process -FilePath $candidate -ArgumentList @('__install-bootstrap', $expectedDigest, $releaseVersion) -NoNewWindow -Wait -PassThru
    if ($placement.ExitCode -ne 0) { Refuse 'placement-failed' 'verified ck could not complete journaled placement' }
    $previousFlag = $env:CK_BOOTSTRAP_WINDOWS_PATH
    try {
        $env:CK_BOOTSTRAP_WINDOWS_PATH = '1'
        $setup = Start-Process -FilePath $destination -ArgumentList 'setup' -NoNewWindow -Wait -PassThru
        if ($setup.ExitCode -ne 0) { Refuse 'setup-failed' "ck setup did not complete; run $destination setup to retry" }
    }
    finally {
        $env:CK_BOOTSTRAP_WINDOWS_PATH = $previousFlag
    }
    Confirm-UserPath -CmdDir $cmdDir
}
finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
