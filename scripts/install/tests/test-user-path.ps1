# Run with pwsh -NoProfile -File scripts/install/tests/test-user-path.ps1.
# No registry provider is used: the real function runs against a registry-key
# double that models REG_EXPAND_SZ reads, even on non-Windows test hosts.
param([string]$Installer = (Join-Path $PSScriptRoot '../install.ps1'))

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw ($errors | Out-String) }
$function = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Ensure-UserPath'
}, $true)
Invoke-Expression $function.Extent.Text

function Refuse { param($Type, $Evidence) throw "refusal: ${Type}: $Evidence" }
function Get-ItemPropertyValue {
    param($Path, $Name, $ErrorAction)
    return [Environment]::ExpandEnvironmentVariables($script:rawPath)
}
function Get-Item {
    param($LiteralPath, $ErrorAction)
    if ($LiteralPath -ne 'HKCU:\Environment') { throw "unexpected key: $LiteralPath" }
    return $script:key
}
function Set-ItemProperty {
    param($Path, $Name, $Value, $Type, $ErrorAction)
    if ($Path -ne 'HKCU:\Environment' -or $Name -ne 'Path') { throw 'unexpected registry write' }
    $script:writtenPath = $Value
    $script:writtenKind = $Type
    $script:writes++
}
$script:key = [pscustomobject]@{}
$script:key | Add-Member ScriptMethod GetValue {
    param($Name, $Default, $Options)
    if ($Name -ne 'Path') { throw 'unexpected value' }
    if ($Options -eq [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) {
        return $script:rawPath
    }
    return [Environment]::ExpandEnvironmentVariables($script:rawPath)
}
$script:key | Add-Member ScriptMethod GetValueKind { param($Name) return $script:kind }
$script:key | Add-Member ScriptMethod Dispose {}

$env:USERPROFILE = 'C:\Fixture\User'
$name = 'user PATH keeps unexpanded registry entries and value kind'
try {
    foreach ($kind in @('ExpandString', 'String')) {
        $script:kind = [Microsoft.Win32.RegistryValueKind]::$kind
        $script:rawPath = '%USERPROFILE%\.dotnet\tools;C:\Other\Bin'
        $script:writtenPath = $null
        $script:writtenKind = $null
        $script:writes = 0
        Ensure-UserPath -BinDir 'C:\CortexKit\bin'
        if ($script:writtenPath -cne '%USERPROFILE%\.dotnet\tools;C:\Other\Bin;C:\CortexKit\bin') {
            throw "raw PATH changed: $script:writtenPath"
        }
        if ($script:writtenKind -ne $script:kind) { throw "registry kind changed: $script:writtenKind" }
        if ($script:writes -ne 1) { throw "unexpected write count: $script:writes" }
    }
    $script:rawPath = '%USERPROFILE%\cortexkit\bin;C:\Other\Bin'
    $script:writes = 0
    Ensure-UserPath -BinDir 'C:\Fixture\User\cortexkit\bin'
    if ($script:writes -ne 0) { throw 'an expanded duplicate was appended' }
    $script:rawPath = $null
    $script:writtenPath = $null
    $script:writtenKind = $null
    Ensure-UserPath -BinDir 'C:\CortexKit\bin'
    if ($script:writtenPath -cne 'C:\CortexKit\bin') { throw 'absent PATH was not initialized' }
    if ($script:writtenKind -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
        throw 'absent PATH did not use REG_EXPAND_SZ'
    }
    Write-Output "ok: $name"
}
catch {
    [Console]::Error.WriteLine("not ok: $name -- $($_.Exception.Message)")
    exit 1
}
