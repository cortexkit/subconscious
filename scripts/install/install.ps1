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

function Ensure-UserPath {
    param([Parameter(Mandatory = $true)][string]$BinDir)

    $environmentKey = 'HKCU:\Environment'
    try {
        $currentPath = Get-ItemPropertyValue -Path $environmentKey -Name Path -ErrorAction SilentlyContinue
    }
    catch {
        Refuse 'path-update-failed' "could not read the user PATH registry value ($($_.Exception.Message))"
    }

    $entries = @()
    if (-not [string]::IsNullOrWhiteSpace($currentPath)) {
        $entries = @($currentPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    $containsBinDir = $false
    foreach ($entry in $entries) {
        if ([string]::Equals($entry, $BinDir, [System.StringComparison]::OrdinalIgnoreCase)) {
            $containsBinDir = $true
            break
        }
    }
    if ($containsBinDir) {
        return
    }

    $updatedPath = @($entries + $BinDir) -join ';'
    try {
        Set-ItemProperty -Path $environmentKey -Name Path -Value $updatedPath -ErrorAction Stop
    }
    catch {
        Refuse 'path-update-failed' "could not update HKCU\\Environment PATH ($($_.Exception.Message))"
    }
}

function Write-InstallerManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Manifest,
        [Parameter(Mandatory = $true)][string]$Binary,
        [Parameter(Mandatory = $true)][string]$BinaryDigest,
        [Parameter(Mandatory = $true)][string]$ArchiveDigest,
        [Parameter(Mandatory = $true)][string]$BinDir,
        [Parameter(Mandatory = $true)][string]$Arch
    )

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
                path = $BinDir
            },
            [ordered]@{
                kind = 'ownership-record'
                path = $Manifest
            }
        )
    }
    $content = $manifestObject | ConvertTo-Json -Depth 5
    $temporaryManifest = "$Manifest.tmp"
    try {
        # Windows PowerShell 5.1's `-Encoding UTF8` writes a byte-order mark,
        # and the JSON reader in ck refuses a document that starts with one
        # ("expected value at line 1 column 1"), so every later ck command
        # refused the inventory. Write the bytes with an explicit BOM-less
        # encoder, which behaves the same on 5.1 and on PowerShell 7.
        [System.IO.File]::WriteAllBytes($temporaryManifest, [System.Text.UTF8Encoding]::new($false).GetBytes($content))
        if ((Test-Path -LiteralPath $Manifest) -and ([System.IO.File]::ReadAllText($Manifest) -eq [System.IO.File]::ReadAllText($temporaryManifest))) {
            Remove-Item -LiteralPath $temporaryManifest -Force -ErrorAction Stop
            return
        }
        Move-Item -LiteralPath $temporaryManifest -Destination $Manifest -Force -ErrorAction Stop
    }
    catch {
        if (Test-Path -LiteralPath $temporaryManifest) {
            Remove-Item -LiteralPath $temporaryManifest -Force -ErrorAction SilentlyContinue
        }
        Refuse 'inventory-record-failed' "could not write $Manifest ($($_.Exception.Message))"
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
$binDir = Join-Path $dataDir 'bin'
$destination = Join-Path $binDir 'ck.exe'
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
            New-Item -ItemType Directory -Path $binDir -Force -ErrorAction Stop | Out-Null
            $temporaryDestination = Join-Path $binDir '.ck.exe.tmp'
            Copy-Item -LiteralPath $candidate -Destination $temporaryDestination -Force -ErrorAction Stop
            Move-Item -LiteralPath $temporaryDestination -Destination $destination -Force -ErrorAction Stop
        }
        catch {
            Refuse 'placement-failed' "could not place ck at $destination ($($_.Exception.Message))"
        }
        Write-Output "Installed ck at $destination."
    }

    Ensure-UserPath -BinDir $binDir
    Write-InstallerManifest -Manifest $manifest -Binary $destination -BinaryDigest $candidateDigest -ArchiveDigest $expectedDigest -BinDir $binDir -Arch $arch
    Write-Output 'Next: ck setup'
}
finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
