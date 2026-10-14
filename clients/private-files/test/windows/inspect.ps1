param([ValidateSet('inspect','fixture','probe','environment')][string]$Mode)
$ErrorActionPreference = 'Stop'
# Windows PowerShell must not inherit PowerShell 7's module search path.
$env:PSModulePath = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/Modules'
function ReadSecurity([string]$LiteralPath) {
  $sections = [Security.AccessControl.AccessControlSections]::Owner -bor [Security.AccessControl.AccessControlSections]::Access
  if ([IO.Directory]::Exists($LiteralPath)) {
    return [Security.AccessControl.DirectorySecurity]::new($LiteralPath, $sections)
  }
  return [Security.AccessControl.FileSecurity]::new($LiteralPath, $sections)
}
function WriteSecurity([string]$LiteralPath, $Security) {
  if ([IO.Directory]::Exists($LiteralPath)) { [IO.Directory]::SetAccessControl($LiteralPath, $Security) }
  else { [IO.File]::SetAccessControl($LiteralPath, $Security) }
}
$inputData = [Console]::In.ReadToEnd() | ConvertFrom-Json
if ($Mode -eq 'environment') {
  [pscustomobject]@{
    os = (Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber)
    filesystem = (Get-Volume -DriveLetter $env:SystemDrive.Substring(0,1) | Select-Object FileSystem,FileSystemType)
    powershell = $PSVersionTable.PSVersion.ToString()
    processArch = $env:PROCESSOR_ARCHITECTURE
  } | ConvertTo-Json -Depth 8
  exit
}
$path = $inputData.path
if ($Mode -eq 'inspect') {
  $security = ReadSecurity $path
  $raw = [System.Security.AccessControl.RawSecurityDescriptor]::new($security.GetSecurityDescriptorBinaryForm(), 0)
  $aces = @()
  if ($null -ne $raw.DiscretionaryAcl) {
    foreach ($ace in $raw.DiscretionaryAcl) {
      $aces += [pscustomobject]@{ type = [int]$ace.AceType; flags = [int]$ace.AceFlags; mask = $ace.AccessMask; sid = $ace.SecurityIdentifier.Value }
    }
  }
  [pscustomobject]@{
    owner = $raw.Owner.Value
    currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    defaultOwner = [System.Security.Principal.WindowsIdentity]::GetCurrent().Owner.Value
    control = [int]$raw.ControlFlags
    aces = if ($null -eq $raw.DiscretionaryAcl) { $null } else { ,$aces }
  } | ConvertTo-Json -Depth 8
  exit
}
if ($Mode -eq 'fixture') {
  if ($inputData.kind -eq 'junction') {
    New-Item -ItemType Junction -Path $path -Target $inputData.target | Out-Null
    Write-Output '{}'; exit
  }
  $security = ReadSecurity $path
  $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
  switch ($inputData.kind) {
    'userOwner' {
      $name = 'ckpfowner' + [Guid]::NewGuid().ToString('N').Substring(0, 10)
      $password = ConvertTo-SecureString ('aA1!' + [Guid]::NewGuid().ToString('N')) -AsPlainText -Force
      $created = $false
      try {
        $user = New-LocalUser -Name $name -Password $password -AccountNeverExpires
        $created = $true
        Add-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-545').Name -Member $name
        & icacls $path /setowner "*$($user.SID.Value)" | Out-Null
        if ($LASTEXITCODE) { throw 'Could not set non-trusted user owner fixture' }
        @{ sid = $user.SID.Value } | ConvertTo-Json
      } finally {
        if ($created) { Remove-LocalUser -Name $name -ErrorAction Stop }
      }
      exit
    }
    'usersModify' {
      $users = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
      $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($users, 'Modify', 'Allow'))
    }
    'usersDeleteChild' {
      $users = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
      $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($users, 'DeleteSubdirectoriesAndFiles', 'Allow'))
    }
    'foreign' {
      $everyone = [System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')
      $security.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($everyone, 'FullControl', 'Allow'))
    }
    'inheritableForeign' {
      $everyone = [System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')
      $security.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($everyone, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    }
    'inherited' {
      $security.SetAccessRuleProtection($false, $false)
      foreach ($rule in $security.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier])) {
        $security.RemoveAccessRuleSpecific($rule)
      }
    }
    'defaultOwner' { $security.SetOwner([System.Security.Principal.WindowsIdentity]::GetCurrent().Owner) }
    'empty' { $security.SetSecurityDescriptorSddlForm('D:P', [Security.AccessControl.AccessControlSections]::Access) }
    'null' {
      Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NullDaclFixture {
  [DllImport("advapi32.dll", CharSet=CharSet.Unicode)]
  public static extern uint SetNamedSecurityInfo(string path, uint type, uint info, IntPtr owner, IntPtr group, IntPtr dacl, IntPtr sacl);
}
'@
      $errorCode = [NullDaclFixture]::SetNamedSecurityInfo($path, 1, [uint32]2147483652, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero)
      if ($errorCode) { throw "Null DACL fixture failed: $errorCode" }
      Write-Output '{}'; exit
    }
    'owner' {
      & icacls $path /setowner '*S-1-5-32-545' | Out-Null
      if ($LASTEXITCODE) { throw 'Could not set foreign BUILTIN\Users owner fixture' }
      Write-Output '{}'; exit
    }
    'administratorsOwner' {
      & icacls $path /setowner '*S-1-5-32-544' | Out-Null
      if ($LASTEXITCODE) { throw 'Could not set Administrators owner fixture' }
      Write-Output '{}'; exit
    }
    default { throw 'Unknown fixture' }
  }
  WriteSecurity $path $security
  Write-Output '{}'
  exit
}
if ($Mode -eq 'probe') {
  $probeClock = [Diagnostics.Stopwatch]::StartNew()
  $name = 'ckpf' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
  $password = ConvertTo-SecureString ('aA1!' + [Guid]::NewGuid().ToString('N')) -AsPlainText -Force
  $public = Join-Path $env:PUBLIC ('ck-private-probe-' + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory $public | Out-Null
  $userCreated = $false
  try {
    New-LocalUser -Name $name -Password $password -AccountNeverExpires | Out-Null
    $userCreated = $true
    $users = (Get-LocalGroup -SID 'S-1-5-32-545').Name
    Add-LocalGroupMember -Group $users -Member $name
    $security = [System.Security.AccessControl.DirectorySecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    $everyone = [System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')
    $security.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($everyone, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    WriteSecurity $public $security
    $control = Join-Path $public 'control.txt'
    [IO.File]::WriteAllText($control, 'public-control')
    $request = Join-Path $public 'request.json'
    $result = Join-Path $public 'result.json'
    $runtimeCopy = Join-Path $public (Split-Path -Leaf $inputData.runtime)
    Copy-Item -LiteralPath $inputData.runtime -Destination $runtimeCopy
    $runtimeHash = (Get-FileHash -LiteralPath $inputData.runtime -Algorithm SHA256).Hash
    if ((Get-FileHash -LiteralPath $runtimeCopy -Algorithm SHA256).Hash -ne $runtimeHash) { throw 'Probe runtime executable hash mismatch' }
    @{ control = $control; file = $inputData.file; directory = $inputData.directory; result = $result; runtime = $runtimeCopy; writerScript = (Join-Path $public 'standard-writer.mjs') } | ConvertTo-Json | Set-Content -LiteralPath $request
    $package = Join-Path $public 'package'
    New-Item -ItemType Directory $package | Out-Null
    $packageRoot = Join-Path $PSScriptRoot '../..'
    Copy-Item -LiteralPath (Join-Path $packageRoot 'dist') -Destination $package -Recurse
    Copy-Item -LiteralPath (Join-Path $packageRoot 'prebuilds') -Destination $package -Recurse
    Copy-Item -LiteralPath (Join-Path $packageRoot 'package.json') -Destination $package
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'standard-writer.mjs') -Destination $public
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'inspect.ps1') -Destination $public
    $script = Join-Path $public 'probe.ps1'
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'second-user.ps1') -Destination $script
    $credential = [PSCredential]::new("$env:COMPUTERNAME\$name", $password)
    $stdout = Join-Path $public 'stdout.txt'
    $stderr = Join-Path $public 'stderr.txt'
    $provisionMs = $probeClock.ElapsedMilliseconds
    try {
      $process = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Credential $credential -ArgumentList @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', "`"$script`"", '-Request', "`"$request`"") -WorkingDirectory $public -LoadUserProfile -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
      $null = $process.Handle
      if (!$process.WaitForExit(45000)) { $process.Kill(); throw 'Second-user probe timed out' }
      $process.WaitForExit()
    } catch {
      throw "Second-user launch/wait failed (userCreated=$userCreated, LoadUserProfile=true, directory=$public): $($_.Exception.ToString())"
    }
    if ($process.ExitCode -ne 0 -or !(Test-Path -LiteralPath $result)) {
      $out = if (Test-Path $stdout) { Get-Content -Raw $stdout } else { '<no stdout>' }
      $err = if (Test-Path $stderr) { Get-Content -Raw $stderr } else { '<no stderr>' }
      throw "Second-user probe failed (exitCode=$($process.ExitCode), userCreated=$userCreated, LoadUserProfile=true, resultExists=$(Test-Path -LiteralPath $result)): stdout=$out stderr=$err"
    }
    $report = Get-Content -Raw -LiteralPath $result | ConvertFrom-Json
    if (!$report.standardUser -or !$report.controlRead -or !$report.fileDenied -or !$report.directoryDenied -or !$report.standardWriter) { throw "Second-user denial/control failed: $($report | ConvertTo-Json -Compress)" }
    $report | Add-Member -NotePropertyName runtimeSha256 -NotePropertyValue $runtimeHash
    $report | Add-Member -NotePropertyName provisionMs -NotePropertyValue $provisionMs
    $report | Add-Member -NotePropertyName launchAndProbeMs -NotePropertyValue ($probeClock.ElapsedMilliseconds - $provisionMs)
    $report | ConvertTo-Json
  } finally {
    if ($userCreated) { Remove-LocalUser -Name $name -ErrorAction Stop }
    Remove-Item -LiteralPath $public -Force -Recurse -ErrorAction SilentlyContinue
  }
}
