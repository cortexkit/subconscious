# Run under native Windows PowerShell. No daemon, network or real PATH write.
$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ("ck-journal-smoke-" + [guid]::NewGuid().ToString('N'))
$env:LOCALAPPDATA = $root
$env:PROCESSOR_ARCHITECTURE = 'AMD64'
$env:PROCESSOR_ARCHITEW6432 = ''
$archive = [Text.Encoding]::UTF8.GetBytes('verified test archive')
$sha = [BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($archive)).Replace('-', '').ToLowerInvariant()
$global:CkSmokeCalls = @()
$global:CkSmokeRawPath = 'C:\Fixture\foreign'
function Invoke-WebRequest {
    param($Uri, $OutFile, [switch]$UseBasicParsing, $ErrorAction)
    if ($Uri -like '*index.json') {
        $content = @{ components = @{ core = @{ version = '1.2.3'; assets = @{ 'windows-x64' = @{ ck = @{ url = 'https://fixture.invalid/archive.zip'; sha256 = $sha } } } } } }
        $content | ConvertTo-Json -Depth 8 | Microsoft.PowerShell.Management\Set-Content -LiteralPath $OutFile
    }
    else { [IO.File]::WriteAllBytes($OutFile, $archive) }
}
function Expand-Archive {
    param($LiteralPath, $DestinationPath, [switch]$Force, $ErrorAction)
    [IO.Directory]::CreateDirectory($DestinationPath) | Out-Null
    [IO.File]::WriteAllText((Join-Path $DestinationPath 'ck.exe'), 'never executed')
}
function Get-Item {
    param($LiteralPath, [switch]$Force, $ErrorAction)
    if ($LiteralPath -notlike 'HK*') { return Microsoft.PowerShell.Management\Get-Item -LiteralPath $LiteralPath -Force:$Force -ErrorAction $ErrorAction }
    $key = [pscustomobject]@{ User = ($LiteralPath -like 'HKCU:*') }
    $key | Add-Member ScriptMethod GetValueNames { @('Path') }
    $key | Add-Member ScriptMethod GetValueKind { [Microsoft.Win32.RegistryValueKind]::ExpandString }
    $key | Add-Member ScriptMethod GetValue { param($name, $default, $options) if ($this.User -and $name -eq 'Path') { $global:CkSmokeRawPath } else { $null } }
    $key | Add-Member ScriptMethod Dispose {}
    $key
}
function Start-Process {
    param($FilePath, [string[]]$ArgumentList, [switch]$NoNewWindow, [switch]$Wait, [switch]$PassThru)
    $destination = Join-Path $root 'cortexkit\cmd\ck.exe'
    if ($ArgumentList[0] -eq '__install-bootstrap') {
        if ($ArgumentList[1] -cne $sha -or $ArgumentList[2] -cne '1.2.3' -or $FilePath -notlike '*\extracted\ck.exe') { throw 'wrong extracted helper arguments' }
        $global:CkSmokeCalls += 'handover'
        [IO.Directory]::CreateDirectory((Split-Path -Parent $destination)) | Out-Null
        [IO.File]::Copy($FilePath, $destination, $true)
    }
    else {
        if ($ArgumentList[0] -cne 'setup' -or $FilePath -ine $destination -or $env:CK_BOOTSTRAP_WINDOWS_PATH -cne '1') { throw "wrong installed setup arguments: $FilePath, $ArgumentList, flag=$env:CK_BOOTSTRAP_WINDOWS_PATH" }
        $global:CkSmokeCalls += 'setup'
        $global:CkSmokeRawPath = (Split-Path -Parent $destination) + ';' + $global:CkSmokeRawPath
    }
    [pscustomobject]@{ ExitCode = 0 }
}
function Set-ItemProperty { throw 'installer must not write to the registry' }
try {
    & (Join-Path $PSScriptRoot '../install.ps1')
    if (($global:CkSmokeCalls -join ',') -cne 'handover,setup') { throw "wrong handover order: $global:CkSmokeCalls" }
    if ([IO.File]::Exists((Join-Path $root 'cortexkit\installer-manifest.bootstrap.json'))) { throw 'the script wrote a sidecar' }
    'PASS: verified Windows download, private extraction, one Rust handover and installed setup; no real registry writes'
}
finally { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
