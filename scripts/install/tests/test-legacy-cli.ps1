# Offline filesystem and registry-model checks; runs on PowerShell 5.1 and 7.
param([string]$Installer = (Join-Path $PSScriptRoot '../install.ps1'))
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
foreach ($function in $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true)) {
    Invoke-Expression $function.Extent.Text
}
function Refuse { param($Type, $Evidence) throw "refusal: ${Type}: $Evidence" }
function Broadcast-EnvironmentChange {}
function Get-Item {
    param($LiteralPath, $ErrorAction, [switch]$Force)
    if ($LiteralPath -eq 'HKCU:\Environment') { return $script:userKey }
    if ($LiteralPath -like 'HKLM:*') { return $script:machineKey }
    return Microsoft.PowerShell.Management\Get-Item -LiteralPath $LiteralPath -Force:$Force
}
function Set-ItemProperty {
    param($Path, $Name, $Value, $Type, $ErrorAction)
    if ($Path -ne 'HKCU:\Environment' -or $Name -ne 'Path') { throw 'unexpected registry write' }
    $script:userKey.Path = $Value
}
function New-RegistryKey {
    param([string]$Path)
    $key = [pscustomobject]@{ Path = $Path; PATHEXT = '.COM;.EXE;.BAT;.CMD' }
    $key | Add-Member ScriptMethod GetValue { param($Name, $Default, $Options) return $this.$Name }
    $key | Add-Member ScriptMethod GetValueKind { param($Name) return [Microsoft.Win32.RegistryValueKind]::ExpandString }
    $key | Add-Member ScriptMethod Dispose {}
    return $key
}
$root = Join-Path ([IO.Path]::GetTempPath()) ("ck-legacy-tests-" + [Guid]::NewGuid().ToString('N'))
$originalPath = $env:Path
try {
    $data = Join-Path $root 'cortexkit'
    $bin = Join-Path $data 'bin'
    $cmd = Join-Path $data 'cmd'
    $other = Join-Path $root 'earlier'
    $machine = Join-Path $root 'machine'
    foreach ($dir in @($bin, $cmd, $other, $machine)) { New-Item -ItemType Directory $dir -Force | Out-Null }
    $destination = Join-Path $cmd 'ck.exe'
    [IO.File]::WriteAllText($destination, 'new')
    [IO.File]::WriteAllText((Join-Path $bin 'ck.exe'), 'old')
    [IO.File]::WriteAllText((Join-Path $bin 'ck-subc.exe'), 'module')
    [IO.File]::WriteAllText((Join-Path $other 'ck.cmd'), 'old wrapper')
    $script:machineKey = New-RegistryKey ''
    # The old module directory precedes cmd, as in an upgraded Windows profile.
    $script:userKey = New-RegistryKey "$bin;$cmd"
    $script:Retirements = @()
    Retire-LegacyClis -DataDir $data -Destination $destination
    Ensure-UserPath -CmdDir $cmd
    $script:userKey.Path = "$other;$bin;$cmd"
    Retire-LegacyClis -DataDir $data -Destination $destination
    Ensure-UserPath -CmdDir $cmd
    if (-not (Test-Path (Join-Path $other 'ck.cmd')) -or [IO.File]::ReadAllText((Join-Path $other 'ck.cmd')) -cne 'old wrapper') { throw 'foreign_cli_untouched: foreign CLI moved or changed' }
    Write-Output 'ok: foreign_cli_untouched'
    if (Test-Path (Join-Path $bin 'ck.exe')) { throw 'legacy_cli_retirement: old CLI remains' }
    if (-not (Test-Path (Join-Path $bin 'ck-subc.exe'))) { throw 'legacy_cli_retirement: module removed' }
    if ($script:Retirements.Count -ne 1) { throw 'legacy_cli_retirement: missing retirement row' }
    $manifest = Join-Path $data 'installer-manifest.json'
    Write-InstallerManifest -Manifest $manifest -Binary $destination -BinaryDigest ('a' * 64) -ArchiveDigest ('b' * 64) -CmdDir $cmd -Arch x64
    $rows = @((Get-Content $manifest -Raw | ConvertFrom-Json).mutations | Where-Object { $_.kind -eq 'legacy-cli-retirement' })
    if ($rows.Count -ne 1) { throw 'legacy_cli_retirement: missing manifest row' }
    foreach ($row in $rows) {
        if (-not (Test-Path -LiteralPath $row.backup_path) -or $row.backup_path -notmatch '\.retired-\d{8}T\d{6}Z-') { throw 'legacy_cli_retirement: backup missing' }
        if ($row.original_path -ine (Join-Path $bin 'ck.exe') -or $row.path -ine $row.backup_path -or $row.sha256 -cne (Get-Sha256 $row.backup_path)) { throw 'legacy_cli_retirement: recorded path or digest differs' }
    }
    if ($script:userKey.Path -notlike "*$bin*") { throw 'legacy_cli_retirement: bin removed from PATH' }
    Write-Output 'ok: legacy_cli_retirement'

    $script:userKey.Path = $other
    Retire-LegacyClis -DataDir $data -Destination $destination
    Ensure-UserPath -CmdDir $cmd
    if (-not (Test-Path (Join-Path $other 'ck.cmd')) -or [IO.File]::ReadAllText((Join-Path $other 'ck.cmd')) -cne 'old wrapper') { throw 'no_cmd_foreign_untouched: external CLI renamed while cmd absent' }
    Write-Output 'ok: no_cmd_foreign_untouched'

    # Exercise repair independently of retirement, including PATHEXT priority.
    [IO.File]::WriteAllText((Join-Path $other 'ck.com'), 'shadow')
    $script:userKey.Path = "$other;$bin;$cmd"
    Ensure-UserPath -CmdDir $cmd
    $fresh = Get-FreshShellEnvironment
    if ((Resolve-CkApplication $fresh.Path $fresh.PathExt) -ine $destination) { throw 'fresh_shell_path_repair: wrong winner' }
    if (($script:userKey.Path -split ';')[0] -ine $cmd) { throw 'fresh_shell_path_repair: cmd not first' }
    Write-Output 'ok: fresh_shell_path_repair'

    $shadow = Join-Path $machine 'ck.cmd'
    [IO.File]::WriteAllText($shadow, 'machine shadow')
    $script:machineKey.Path = $machine
    $refusal = $null
    try { Retire-LegacyClis -DataDir $data -Destination $destination; Ensure-UserPath -CmdDir $cmd } catch { $refusal = $_.Exception.Message }
    if ($null -eq $refusal -or -not $refusal.Contains('cli-shadowed') -or -not $refusal.Contains($shadow) -or -not $refusal.Contains("PATH entry $machine")) { throw 'machine_shadow_refusal: missing named shadow and PATH entry' }
    if (-not (Test-Path $shadow) -or [IO.File]::ReadAllText($shadow) -cne 'machine shadow') { throw 'machine_shadow_refusal: foreign machine CLI moved or changed' }
    Write-Output 'ok: machine_shadow_refusal'

    $aliasData = Join-Path $root 'alias-data'
    $aliasCmd = Join-Path $aliasData 'cmd'
    New-Item -ItemType Directory $aliasCmd -Force | Out-Null
    $aliasDestination = Join-Path $aliasCmd 'ck.exe'
    [IO.File]::WriteAllText($aliasDestination, 'new')
    $linkType = 'SymbolicLink'
    if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) { $linkType = 'Junction' }
    $aliasBin = Join-Path $aliasData 'bin'
    New-Item -ItemType $linkType -Path $aliasBin -Target $aliasCmd | Out-Null
    $script:machineKey.Path = ''
    $script:userKey.Path = "$aliasBin;$aliasCmd"
    $count = $script:Retirements.Count
    try {
        Retire-LegacyClis -DataDir $aliasData -Destination $aliasDestination
        Ensure-UserPath -CmdDir $aliasCmd
    }
    catch { throw "cli_directory_alias: $($_.Exception.Message)" }
    if (-not (Test-Path $aliasDestination) -or $script:Retirements.Count -ne $count) { throw 'cli_directory_alias: retired the new destination through bin' }
    Write-Output 'ok: cli_directory_alias'

    Write-Output '6 legacy CLI checks passed'
}
finally {
    $env:Path = $originalPath
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
