param([ValidateSet('node','bun')][string]$Runtime, [int]$ExpectedCount = 21, [ValidatePattern('^[a-z0-9-]*$')][string]$EvidenceSuffix = '', [string]$ExpectedVersion = '')
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$env:NO_COLOR = '1'
$version = & $Runtime --version
if ($LASTEXITCODE) { throw "Cannot query $Runtime version" }
Write-Output "$Runtime --version: $version"
if ($Runtime -eq 'bun' -and (!$ExpectedVersion -or $version -ne $ExpectedVersion)) {
  throw "required_bun_version: expected '$ExpectedVersion', got '$version'; setup must succeed before qualification"
}
if ($Runtime -eq 'node' -and $version -notmatch '^v24\.') { throw "Node 24 required; got $version" }
$arch = & $Runtime -p 'process.arch'
if ($LASTEXITCODE) { throw "Cannot query $Runtime architecture" }
New-Item -ItemType Directory -Force private-files-evidence | Out-Null
$log = Join-Path 'private-files-evidence' "$Runtime-$version-$arch$EvidenceSuffix-tests.txt"
$arguments = if ($Runtime -eq 'node') {
  @('--test', '--test-reporter=tap', './test/windows/windows-acl.mjs')
} else {
  @('test', '--timeout', '120000', './test/windows/windows-acl.mjs')
}
& $Runtime @arguments 2>&1 | Tee-Object -FilePath $log
$exitCode = $LASTEXITCODE
if ($exitCode) { throw "Required $Runtime ACL tests failed (exit $exitCode); see $log" }
$output = [IO.File]::ReadAllText((Join-Path (Get-Location) $log))
$output = [regex]::Replace($output, '\x1b\[[0-9;]*m', '')
function ReadCounter([string]$Pattern) {
  $matches = [regex]::Matches($output, $Pattern)
  if ($matches.Count -ne 1) { throw "required_real_acl_test_count: missing or ambiguous runtime counter ($Pattern)" }
  return [int]$matches[0].Groups[1].Value
}
if ($Runtime -eq 'node') {
  $count = ReadCounter '(?m)^# tests (\d+)\s*$'
  $passed = ReadCounter '(?m)^# pass (\d+)\s*$'
  $failed = ReadCounter '(?m)^# fail (\d+)\s*$'
  $skipped = ReadCounter '(?m)^# skipped (\d+)\s*$'
} else {
  $count = ReadCounter '(?m)^Ran (\d+) tests? across '
  $passed = ReadCounter '(?m)^\s*(\d+) pass\s*$'
  $failed = ReadCounter '(?m)^\s*(\d+) fail\s*$'
  $skipMatches = [regex]::Matches($output, '(?m)^\s*(\d+) skip\s*$')
  $skipped = if ($skipMatches.Count) { [int]$skipMatches[0].Groups[1].Value } else { 0 }
}
if ($count -ne $ExpectedCount -or $passed -ne $ExpectedCount -or $failed -ne 0 -or $skipped -ne 0) {
  throw "required_real_acl_test_count: expected $ExpectedCount executed non-skipped tests; got tests=$count pass=$passed fail=$failed skip=$skipped"
}
Write-Output "PASS required_real_acl_test_count: $count tests/$passed pass/$failed fail/$skipped skip ($Runtime $version, $arch)"
