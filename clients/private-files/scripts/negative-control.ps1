$ErrorActionPreference = 'Stop'
$source = 'native/addon.c'
$original = [IO.File]::ReadAllText((Join-Path (Get-Location) $source))
if ((git diff -- $source) -or (git diff --cached -- $source)) { throw 'Negative control requires clean native source' }
# Stage the unchanged source so reverting the verifier mutation cannot discard existing implementation changes.
git add -- $source
if (git diff --stat) { throw 'Negative control requires an empty working diff' }
try {
  $mutant = $original.Replace('if (valid && canonical) {', 'if (0 && valid && canonical) { /* NON-VACUITY BREAK */')
  if ($mutant -eq $original) { throw 'Verifier mutation did not match' }
  [IO.File]::WriteAllText((Join-Path (Get-Location) $source), $mutant)
  $during = git diff --stat
  if (!$during) { throw 'No mutation diff recorded' }
  $during | Tee-Object -FilePath private-files-evidence/negative-during.txt
  & ./native/build.ps1
  $env:PRIVATE_FILES_NEGATIVE_CONTROL = '1'
  & node --test --test-reporter=tap '--test-name-pattern=^foreign-grant fixture refuses before reading bytes$' test/windows/windows-acl.mjs 2>&1 | Tee-Object -FilePath private-files-evidence/negative-test.txt
  $code = $LASTEXITCODE
  if ($code -eq 0) { throw 'Disabled verifier was not detected by the foreign-grant test' }
  $output = [IO.File]::ReadAllText((Join-Path (Get-Location) 'private-files-evidence/negative-test.txt'))
  if ($output -notmatch 'not ok.*foreign-grant fixture refuses before reading bytes' -or $output -notmatch 'Missing expected rejection' -or $output -notmatch '(?m)^# fail 1\s*$') { throw 'Negative control failed for an unrelated reason' }
} finally {
  git checkout -- $source
  (Get-Item $source).LastWriteTime = Get-Date
  Remove-Item Env:PRIVATE_FILES_NEGATIVE_CONTROL -ErrorAction SilentlyContinue
  $after = git diff --stat
  if ($after) { throw "Mutation restoration left changes: $after" }
  'empty git diff --stat after restore' | Set-Content private-files-evidence/negative-after.txt
  & ./native/build.ps1
}

$harness = 'test/windows/windows-acl.mjs'
$originalHarness = [IO.File]::ReadAllText((Join-Path (Get-Location) $harness))
$originalArtifacts = $env:PRIVATE_FILES_ARTIFACTS
git add -- $harness
if (git diff --stat) { throw 'Test-count control requires an empty working diff' }
try {
  $skipped = $originalHarness.Replace("`ntest(", "`nnodeTest.skip(") + "`n// NON-VACUITY BREAK`n"
  if ($skipped -eq $originalHarness -or $skipped -notmatch 'test.skip\(') { throw 'Test-count mutation did not match' }
  [IO.File]::WriteAllText((Join-Path (Get-Location) $harness), $skipped)
  $during = git diff --stat
  if (!$during) { throw 'No test-count mutation diff recorded' }
  $during | Tee-Object -FilePath private-files-evidence/count-negative-during.txt
  $caught = $false
  $env:PRIVATE_FILES_ARTIFACTS = 'private-files-evidence/count-negative'
  try {
    & ./test/windows/run-acl.ps1 -Runtime node -EvidenceSuffix '-count-negative'
  } catch {
    if ($_.Exception.Message -notmatch 'required_real_acl_test_count: expected 21 executed non-skipped tests; got tests=21 pass=0 fail=0 skip=21') { throw }
    $_.Exception.Message | Tee-Object -FilePath private-files-evidence/count-negative-test.txt
    $caught = $true
  }
  if (!$caught) { throw 'Skipping all ACL assertions escaped the real-test-count guard' }
} finally {
  if ($null -eq $originalArtifacts) { Remove-Item Env:PRIVATE_FILES_ARTIFACTS -ErrorAction SilentlyContinue }
  else { $env:PRIVATE_FILES_ARTIFACTS = $originalArtifacts }
  git checkout -- $harness
  (Get-Item $harness).LastWriteTime = Get-Date
  if (git diff --stat) { throw 'Test-count mutation restoration left changes' }
  'empty git diff --stat after restore' | Set-Content private-files-evidence/count-negative-after.txt
}

$runner = 'test/windows/run-acl.ps1'
$originalRunner = [IO.File]::ReadAllText((Join-Path (Get-Location) $runner))
$actualBun = & bun --version
if ($LASTEXITCODE) { throw 'Cannot query Bun for version negative control' }
git add -- $runner
if (git diff --stat) { throw 'Runtime-version control requires an empty working diff' }
try {
  $anchor = 'if ($Runtime -eq ''bun'' -and (!$ExpectedVersion'
  $wrongVersion = $originalRunner.Replace($anchor, '$ExpectedVersion = ''0.0.0'' # NON-VACUITY BREAK' + "`n" + $anchor)
  if ($wrongVersion -eq $originalRunner) { throw 'Runtime-version mutation did not match' }
  [IO.File]::WriteAllText((Join-Path (Get-Location) $runner), $wrongVersion)
  $during = git diff --stat
  if (!$during) { throw 'No runtime-version mutation diff recorded' }
  $during | Tee-Object -FilePath private-files-evidence/version-negative-during.txt
  $caught = $false
  try {
    & ./test/windows/run-acl.ps1 -Runtime bun -ExpectedVersion $actualBun
  } catch {
    if ($_.Exception.Message -notmatch 'required_bun_version: expected ''0.0.0'', got') { throw }
    $_.Exception.Message | Tee-Object -FilePath private-files-evidence/version-negative-test.txt
    $caught = $true
  }
  if (!$caught) { throw 'Wrong Bun version escaped the runtime-version guard' }
} finally {
  git checkout -- $runner
  (Get-Item $runner).LastWriteTime = Get-Date
  if (git diff --stat) { throw 'Runtime-version mutation restoration left changes' }
  'empty git diff --stat after restore' | Set-Content private-files-evidence/version-negative-after.txt
}
