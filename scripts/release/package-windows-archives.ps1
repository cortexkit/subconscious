[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$SourceDirectory,

    [Parameter(Mandatory = $true)]
    [string]$DistDirectory,

    # Asset tuple suffix. The x64 build lands in target/release; a cross-built
    # arm64 lands in target/<triple>/release, and the archive name is the only
    # thing that tells the two apart downstream.
    [ValidateSet("x64", "arm64")]
    [string]$Arch = "x64"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$binaries = @("ck", "ck-subc", "ck-subc-mcp")

# The shipped ck must not carry the test-only release-index key path; it answers
# for its own build shape. Only the host-native x64 binary can be executed on
# the runner; the arm64 lane is cross-built from the same invocation and
# inherits the x64 verdict.
if ($Arch -eq "x64") {
    $ckPath = Join-Path $SourceDirectory "ck.exe"
    $shape = (& $ckPath --ck-build-shape 2>$null | Out-String).Trim()
    if ($shape -ne "test-support: off") {
        throw "Refusing to package ck: build shape is '$shape', expected 'test-support: off'"
    }
}

New-Item -ItemType Directory -Force -Path $DistDirectory | Out-Null

foreach ($binary in $binaries) {
    $sourcePath = Join-Path $SourceDirectory "$binary.exe"
    $archiveName = "$binary-windows-$Arch.zip"
    $archivePath = Join-Path $DistDirectory $archiveName
    $sidecarPath = "$archivePath.sha256"

    if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
        throw "Missing release binary: $sourcePath"
    }

    Remove-Item -LiteralPath $archivePath, $sidecarPath -Force -ErrorAction SilentlyContinue
    Compress-Archive -LiteralPath $sourcePath -DestinationPath $archivePath -Force

    $hash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    # Two spaces and the published basename match `shasum -a 256` sidecars.
    "$hash  $archiveName" | Set-Content -LiteralPath $sidecarPath -Encoding ascii
}
