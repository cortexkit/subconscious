# Bootstrap only ck from the CortexKit release index. Setup owns all runtime
# and configuration work, so this script must never invoke `ck setup`. The
# index signature is not checked here: bootstrap trust is TLS to the index
# host, and the placed `ck` verifies the signed index on its first setup.
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

function Test-FileBytesEqual {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )

    try {
        $leftBytes = [System.IO.File]::ReadAllBytes($Left)
        $rightBytes = [System.IO.File]::ReadAllBytes($Right)
    }
    catch {
        Refuse 'placement-failed' "could not compare $Left and $Right ($($_.Exception.Message))"
    }

    if ($leftBytes.Length -ne $rightBytes.Length) {
        return $false
    }
    for ($index = 0; $index -lt $leftBytes.Length; $index++) {
        if ($leftBytes[$index] -ne $rightBytes[$index]) {
            return $false
        }
    }
    return $true
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

function Retire-LegacyCli {
    param([string]$Old, [string]$Destination, [string]$DataDir)
    if (-not (Test-Path -LiteralPath $Old -PathType Leaf) -or (Test-SameCliFile $Old $Destination)) { return }
    $legacyPath = Join-Path $DataDir 'bin\ck.exe'
    $legacySignature = $Old -ieq $legacyPath
    if ($legacySignature) {
        $linkAttribute = [IO.FileAttributes]::ReparsePoint
        $legacySignature = ((Get-Item -LiteralPath $Old -Force).Attributes -band $linkAttribute) -eq 0 -and
            ((Get-Item -LiteralPath (Split-Path -Parent $Old) -Force).Attributes -band $linkAttribute) -eq 0
    }
    if (-not $legacySignature) { return }
    $backup = "$Old.retired-$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'))-$([Guid]::NewGuid().ToString('N'))"
    $digest = Get-Sha256 -Path $Old
    try { Move-Item -LiteralPath $Old -Destination $backup -ErrorAction Stop }
    catch { Refuse 'cli-shadowed' "could not retire $Old to $backup ($($_.Exception.Message))" }
    $script:Retirements += [ordered]@{ kind = 'legacy-cli-retirement'; path = $backup; original_path = $Old; backup_path = $backup; sha256 = $digest }
    Write-Output "Retired legacy ck at $Old to $backup."
}

function Retire-LegacyClis {
    param([string]$DataDir, [string]$Destination)
    Retire-LegacyCli -Old (Join-Path $DataDir 'bin\ck.exe') -Destination $Destination -DataDir $DataDir
}

function Ensure-UserPath {
    param([Parameter(Mandatory = $true)][string]$CmdDir)

    $environmentKey = 'HKCU:\Environment'
    $key = $null
    $pathKind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    try {
        $key = Get-Item -LiteralPath $environmentKey -ErrorAction Stop
        # The registry provider expands REG_EXPAND_SZ on ordinary reads. Keep
        # the stored placeholders intact when prioritizing our installation path.
        $currentPath = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -ne $currentPath) {
            $pathKind = $key.GetValueKind('Path')
        }
    }
    catch {
        Refuse 'path-update-failed' "could not read the user PATH registry value ($($_.Exception.Message))"
    }
    finally {
        if ($null -ne $key) { $key.Dispose() }
    }

    $entries = @()
    if (-not [string]::IsNullOrWhiteSpace($currentPath)) {
        $entries = @($currentPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    $cmdEntry = $CmdDir
    $kept = @()
    foreach ($entry in $entries) {
        $comparisonEntry = $entry
        if ($pathKind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString) {
            $comparisonEntry = [Environment]::ExpandEnvironmentVariables($entry)
        }
        if ($comparisonEntry.TrimEnd('\', '/') -ieq $CmdDir.TrimEnd('\', '/')) {
            $cmdEntry = $entry
        }
        else {
            $kept += $entry
        }
    }
    $updatedPath = (@($cmdEntry) + $kept) -join ';'
    if ($updatedPath -cne $currentPath) {
        try {
            Set-ItemProperty -Path $environmentKey -Name Path -Value $updatedPath -Type $pathKind -ErrorAction Stop
        }
        catch {
            Refuse 'path-update-failed' "could not update HKCU\\Environment PATH ($($_.Exception.Message))"
        }
        # The PATH is already saved at this point. A failed broadcast only means
        # windows opened before the install may not see it until the next sign-in,
        # so it warns instead of refusing.
        try {
            Broadcast-EnvironmentChange
        }
        catch {
            Write-Warning "PATH was saved, but other open windows were not told about it ($($_.Exception.Message)). Windows opened before this install may need a sign-out to find ck."
        }
    }

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

# A direct registry write preserves both the raw PATH text and its value kind,
# but broadcasting WM_SETTINGCHANGE is what tells Explorer to refresh its copy.
function Broadcast-EnvironmentChange {
    if ([Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
        return
    }

    if (-not ('CortexKit.EnvironmentBroadcast' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace CortexKit {
    public static class EnvironmentBroadcast {
        [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
        public static extern IntPtr SendMessageTimeout(
            IntPtr hWnd, uint message, UIntPtr wParam, string lParam,
            uint flags, uint timeout, out UIntPtr result);
    }
}
'@ -ErrorAction Stop
    }

    $broadcastResult = [UIntPtr]::Zero
    $sent = [CortexKit.EnvironmentBroadcast]::SendMessageTimeout(
        [IntPtr]0xffff, 0x001a, [UIntPtr]::Zero, 'Environment', 0x0002, 5000, [ref]$broadcastResult)
    if ($sent -eq [IntPtr]::Zero) {
        $errorCode = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw [System.ComponentModel.Win32Exception]::new($errorCode)
    }
}

function Write-InstallerManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Manifest,
        [Parameter(Mandatory = $true)][string]$Binary,
        [Parameter(Mandatory = $true)][string]$BinaryDigest,
        [Parameter(Mandatory = $true)][string]$ArchiveDigest,
        [Parameter(Mandatory = $true)][string]$CmdDir,
        [Parameter(Mandatory = $true)][string]$Arch
    )

    $target = $Manifest
    if (Test-Path -LiteralPath $Manifest) {
        $target = Join-Path (Split-Path -Parent $Manifest) 'installer-manifest.bootstrap.json'
    }
    $history = @()
    if (Test-Path -LiteralPath $target) {
        try {
            $history = @((Get-Content -LiteralPath $target -Raw | ConvertFrom-Json).mutations | Where-Object { $null -ne $_.PSObject.Properties['kind'] -and $_.kind -eq 'legacy-cli-retirement' })
        }
        catch { Refuse 'inventory-record-failed' "could not read retirement history in $target ($($_.Exception.Message))" }
    }
    $manifestObject = [ordered]@{
        schema_version = 1
        installer = 'ck'
        platform = "windows-$Arch"
        mutations = @(
            [ordered]@{
                kind = 'binary-placement'
                path = $Binary
                # The binary's bytes (ownership proof) and the archive it came
                # from (what currency checks compare against the index).
                sha256 = $BinaryDigest
                archive_sha256 = $ArchiveDigest
            },
            [ordered]@{
                kind = 'user-path-registry'
                registry_key = 'HKCU\Environment'
                registry_value = 'Path'
                path = $CmdDir
            },
            [ordered]@{
                kind = 'ownership-record'
                path = $Manifest
            }
        ) + $history + @($script:Retirements)
    }
    $content = $manifestObject | ConvertTo-Json -Depth 5
    # On an installed machine `ck setup` has extended the manifest with rows
    # for the daemon, the modules, and the scheduled task, and this script
    # has no way to merge into that without running the ck it just placed.
    # Rewriting the file dropped every one of those rows, after which
    # `ck upgrade` reported the daemon as not installed. An existing manifest
    # is left untouched; the rows go to a sidecar beside it that the next ck
    # to load the inventory folds in and removes.
    $target = $Manifest
    if (Test-Path -LiteralPath $Manifest) {
        $target = Join-Path (Split-Path -Parent $Manifest) 'installer-manifest.bootstrap.json'
    }
    $temporaryManifest = "$target.tmp"
    try {
        # Windows PowerShell 5.1's `-Encoding UTF8` writes a byte-order mark,
        # and the JSON reader in ck refuses a document that starts with one
        # ("expected value at line 1 column 1"), so every later ck command
        # refused the inventory. Write the bytes with an explicit BOM-less
        # encoder, which behaves the same on 5.1 and on PowerShell 7.
        [System.IO.File]::WriteAllBytes($temporaryManifest, [System.Text.UTF8Encoding]::new($false).GetBytes($content))
        if ((Test-Path -LiteralPath $target) -and ([System.IO.File]::ReadAllText($target) -eq [System.IO.File]::ReadAllText($temporaryManifest))) {
            Remove-Item -LiteralPath $temporaryManifest -Force -ErrorAction Stop
            return
        }
        Move-Item -LiteralPath $temporaryManifest -Destination $target -Force -ErrorAction Stop
    }
    catch {
        if (Test-Path -LiteralPath $temporaryManifest) {
            Remove-Item -LiteralPath $temporaryManifest -Force -ErrorAction SilentlyContinue
        }
        Refuse 'inventory-record-failed' "could not write $target ($($_.Exception.Message))"
    }
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
$manifest = Join-Path $dataDir 'installer-manifest.json'
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ck-install-" + [System.Guid]::NewGuid().ToString('N'))
$archivePath = Join-Path $tempDir $archiveName
$extractDir = Join-Path $tempDir 'extracted'
$indexPath = Join-Path $tempDir 'index.json'

try {
    New-Item -ItemType Directory -Path $tempDir -Force -ErrorAction Stop | Out-Null
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
    }
    catch {
        Refuse 'release-incomplete' "ck asset for windows-$arch is missing from $IndexUrl ($($_.Exception.Message))"
    }
    if ([string]::IsNullOrWhiteSpace($archiveUrl) -or $expectedDigest.Length -ne 64) {
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
    $candidateDigest = Get-Sha256 -Path $candidate

    if ((Test-Path -LiteralPath $destination -PathType Leaf) -and (Test-FileBytesEqual -Left $candidate -Right $destination)) {
        Write-Output "ck already matches verified download at $destination; skipping placement."
    }
    else {
        try {
            New-Item -ItemType Directory -Path $cmdDir -Force -ErrorAction Stop | Out-Null
            $temporaryDestination = Join-Path $cmdDir '.ck.exe.tmp'
            Copy-Item -LiteralPath $candidate -Destination $temporaryDestination -Force -ErrorAction Stop
            Move-Item -LiteralPath $temporaryDestination -Destination $destination -Force -ErrorAction Stop
        }
        catch {
            Refuse 'placement-failed' "could not place ck at $destination ($($_.Exception.Message))"
        }
        Write-Output "Installed ck at $destination."
    }

    $script:Retirements = @()
    try {
        Retire-LegacyClis -DataDir $dataDir -Destination $destination
        Ensure-UserPath -CmdDir $cmdDir
    }
    finally {
        Write-InstallerManifest -Manifest $manifest -Binary $destination -BinaryDigest $candidateDigest -ArchiveDigest $expectedDigest -CmdDir $cmdDir -Arch $arch
    }
    Write-Output 'Next: ck setup'
}
finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
