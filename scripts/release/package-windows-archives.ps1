[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$SourceDirectory,

    [Parameter(Mandatory = $true)]
    [string]$DistDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$binaries = @("ck", "ck-subc", "ck-subc-mcp")
New-Item -ItemType Directory -Force -Path $DistDirectory | Out-Null

foreach ($binary in $binaries) {
    $sourcePath = Join-Path $SourceDirectory "$binary.exe"
    $archiveName = "$binary-windows-x64.zip"
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
