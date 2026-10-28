$ErrorActionPreference = 'Stop'
$source = Join-Path (Get-Location) 'native/addon.c'
$original = [IO.File]::ReadAllText($source)
$anchor = 'j->path = extended_path(j->original_path);'
$replacement = 'j->path = _wcsdup(j->original_path);'
if ([regex]::Matches($original, [regex]::Escape($anchor)).Count -ne 1) {
  throw 'Long-path conversion control must match exactly once'
}
$policyKey = 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem'
$policy = Get-ItemProperty -Path $policyKey
$hadPolicy = $policy.PSObject.Properties.Name -contains 'LongPathsEnabled'
$originalPolicy = $policy.LongPathsEnabled
try {
  [IO.File]::WriteAllText($source, $original.Replace($anchor, $replacement))
  $line = ([IO.File]::ReadAllLines($source) | Select-String -SimpleMatch $replacement).LineNumber
  "native/addon.c:${line}: $replacement" | Tee-Object -FilePath private-files-evidence/long-path-negative-applied.txt
  & ./native/build.ps1
  # A new Node process must see legacy Win32 limits regardless of the runner image's default policy.
  New-ItemProperty -Path $policyKey -Name LongPathsEnabled -PropertyType DWord -Value 0 -Force | Out-Null
  "LongPathsEnabled=0 for negative control; original=$originalPolicy" | Tee-Object -FilePath private-files-evidence/long-path-negative-policy.txt
  & node --test --test-reporter=tap '--test-name-pattern=^Windows long paths over 300 characters cover private file lifecycle$' test/windows/long-paths.mjs 2>&1 | Tee-Object -FilePath private-files-evidence/long-path-negative-test.txt
  $code = $LASTEXITCODE
  $output = [IO.File]::ReadAllText((Join-Path (Get-Location) 'private-files-evidence/long-path-negative-test.txt'))
  if ($code -eq 0 -or $output -notmatch 'not ok.*Windows long paths over 300 characters cover private file lifecycle' -or
      $output -notmatch 'Cannot open pinned object' -or $output -notmatch '(?m)^# fail 1\s*$') {
    throw 'Bypassing long-path conversion did not fail exactly the long-path filesystem regression'
  }
} finally {
  [IO.File]::WriteAllText($source, $original)
  if ($hadPolicy) { Set-ItemProperty -Path $policyKey -Name LongPathsEnabled -Value $originalPolicy }
  else { Remove-ItemProperty -Path $policyKey -Name LongPathsEnabled -ErrorAction SilentlyContinue }
  & ./native/build.ps1
}
