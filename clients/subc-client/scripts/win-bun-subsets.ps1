param(
  [Parameter(Mandatory = $true)][ValidateSet('push', 'narrow')][string]$Set,
  [Parameter(Mandatory = $true)][string]$Label,
  [Parameter(Mandatory = $true)][string]$OutputDirectory,
  [int]$Runs = 5
)

# Runs named subsets of bun:test files as fresh processes, $Runs times each, and
# classifies every run. Set 'push' covers the control-push test that times out on
# Bun 1.3.14; set 'narrow' splits tests/managed-client.test.ts to find the
# smallest group that still makes Bun exit abnormally. See win-reconnect-repro.md.

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path $OutputDirectory).Path
$version = (& bun --version).Trim()
if ($LASTEXITCODE -ne 0) { throw 'bun --version failed' }
if ($version -ne $Label) { throw "Expected Bun $Label, found $version" }

$managed = 'tests/managed-client.test.ts'
$pushDiag = 'scripts/win-push-timeout.test.ts'
# The tests that drop a connection and make the client reconnect: the seven in
# "SubcClient managed call" plus the scoped-route reopen. Patterns match Bun's
# full test name, "<describe> <test>", from its start.
$reconnect = 'SubcClient managed call (opens a fresh route for a managed call after reconnect|recovers across a daemon restart|re-attaches the same consumer_identity|isolates a refused route|uses capped reconnect backoff|surfaces a non-transient reconnect refusal|a half-open socket is convicted)|scoped route opens cached scopes are snapshotted'

if ($Set -eq 'push') {
  $rows = @(
    @{ Id = 'real-test-alone'; File = $managed; Pattern = '^daemon-originated control pushes route\.closing reaches the observer'; Expected = 1; OptIn = $false },
    @{ Id = 'real-describe'; File = $managed; Pattern = '^daemon-originated control pushes '; Expected = 2; OptIn = $false },
    @{ Id = 'logged-exact'; File = $pushDiag; Pattern = 'variant=exact$'; Expected = 1; OptIn = $true },
    @{ Id = 'logged-no-push'; File = $pushDiag; Pattern = 'variant=no-push$'; Expected = 1; OptIn = $true },
    @{ Id = 'logged-valid-only'; File = $pushDiag; Pattern = 'variant=valid-only$'; Expected = 1; OptIn = $true },
    @{ Id = 'logged-garbage-first'; File = $pushDiag; Pattern = 'variant=garbage-first$'; Expected = 1; OptIn = $true }
  )
} else {
  $rows = @(
    # Level 0: the whole file in this job, as the comparison point.
    @{ Id = 'L0-full-file'; File = $managed; Pattern = $null; Expected = 48 },
    # Level 1: one describe block per process.
    @{ Id = 'L1-control-pushes'; File = $managed; Pattern = '^daemon-originated control pushes '; Expected = 2 },
    @{ Id = 'L1-capability-resolution'; File = $managed; Pattern = '^SubcClient capability resolution '; Expected = 4 },
    @{ Id = 'L1-subscription-flag'; File = $managed; Pattern = '^subscription request flag '; Expected = 1 },
    @{ Id = 'L1-reverse-request'; File = $managed; Pattern = '^reverse-request lane '; Expected = 3 },
    @{ Id = 'L1-managed-call'; File = $managed; Pattern = '^SubcClient managed call '; Expected = 20 },
    @{ Id = 'L1-module-restart'; File = $managed; Pattern = '^managed route\.open across a module restart '; Expected = 5 },
    @{ Id = 'L1-scoped-routes'; File = $managed; Pattern = '^scoped route opens '; Expected = 13 },
    # Level 2: every reconnecting test versus every other test.
    @{ Id = 'L2-reconnect-tests'; File = $managed; Pattern = "^($reconnect)"; Expected = 8 },
    @{ Id = 'L2-all-but-reconnect'; File = $managed; Pattern = "^(?!$reconnect)"; Expected = 40 },
    # Level 3: each reconnecting test alone.
    @{ Id = 'L3-fresh-route-after-reconnect'; File = $managed; Pattern = '^SubcClient managed call opens a fresh route for a managed call after reconnect'; Expected = 1 },
    @{ Id = 'L3-daemon-restart-key-rotation'; File = $managed; Pattern = '^SubcClient managed call recovers across a daemon restart'; Expected = 1 },
    @{ Id = 'L3-consumer-identity-reattach'; File = $managed; Pattern = '^SubcClient managed call re-attaches the same consumer_identity'; Expected = 1 },
    @{ Id = 'L3-refused-route-isolation'; File = $managed; Pattern = '^SubcClient managed call isolates a refused route'; Expected = 1 },
    @{ Id = 'L3-capped-reconnect-backoff'; File = $managed; Pattern = '^SubcClient managed call uses capped reconnect backoff'; Expected = 1 },
    @{ Id = 'L3-non-transient-refusal'; File = $managed; Pattern = '^SubcClient managed call surfaces a non-transient reconnect refusal'; Expected = 1 },
    @{ Id = 'L3-half-open-conviction'; File = $managed; Pattern = '^SubcClient managed call a half-open socket is convicted'; Expected = 1 },
    @{ Id = 'L3-scoped-reopen'; File = $managed; Pattern = '^scoped route opens cached scopes are snapshotted'; Expected = 1 }
  )
}

$summaryPath = Join-Path $OutputDirectory "$Label-$Set-summary.md"
$header = @(
  "## Bun $version / set $Set / $Runs fresh runs per row",
  '',
  '| Row | Tests | Crash | Timeout | Stuck | Late | Failure | Filter mismatch | Pass | Exit codes | Push outcomes |',
  '| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |'
)
$header | Set-Content -Path $summaryPath

function Get-RunStatus([int]$Code, [string]$Output, [int]$Expected, [string]$File) {
  # Bun's own crash report or a native crash code is a crash, never a failure.
  # Same rule as win-reconnect-repro.ps1; the exit code is recorded either way.
  if (($Code -in @(134, 139, -1073741819, -1073740791, -1073741571, -1073741795)) -or
      ($Output -match 'Bun has crashed|Segmentation fault|panic\([^)]*thread\):')) {
    return 'crash'
  }
  $ran = if ($Output -match 'Ran (\d+) tests? across') { [int]$Matches[1] } else { -1 }
  if ($Output -match 'PUSHDIAG RESULT \S+ outcome=stuck') { return 'stuck' }
  if ($Code -ne 0 -and $Output -match 'timed out after \d+\s*ms') { return 'timeout' }
  if ($Code -ne 0) { return 'failure' }
  # A green run that executed a different number of tests than the row names
  # (a pattern that stopped matching, a skipped opt-in) must not count as a pass.
  if ($ran -ne $Expected) { return 'filter-mismatch' }
  # Bun counts skipped tests in "Ran N tests", so a missing WIN_BUN_REPRO opt-in
  # would otherwise look like a pass; the logged diagnostic must print its result.
  if ($Output -match '(?m)^\s*[1-9]\d* skip\s*$') { return 'filter-mismatch' }
  if ($File -like '*win-push-timeout*' -and $Output -notmatch 'PUSHDIAG RESULT') { return 'filter-mismatch' }
  if ($Output -match 'PUSHDIAG RESULT \S+ outcome=late') { return 'late' }
  return 'pass'
}

$failedRows = 0
Push-Location (Split-Path $PSScriptRoot -Parent)
$savedOptIn = $env:WIN_BUN_REPRO
try {
  foreach ($row in $rows) {
    Write-Host "::group::Bun $version / $Set / $($row.Id)"
    $env:WIN_BUN_REPRO = if ($row.OptIn) { '1' } else { $savedOptIn }
    $arguments = @('test', $row.File)
    if ($row.Pattern) { $arguments += @('-t', $row.Pattern) }
    $attempts = @()
    for ($run = 1; $run -le $Runs; $run++) {
      $log = Join-Path $OutputDirectory "$Label-$Set-$($row.Id)-run-$run.log"
      Write-Host "Run $run/$Runs`: bun $($arguments -join ' ')"
      & bun @arguments 2>&1 | Tee-Object -FilePath $log | Out-Host
      $code = $LASTEXITCODE
      $output = (Get-Content -Raw $log) -replace '\x1B\[[0-?]*[ -/]*[@-~]', ''
      $status = Get-RunStatus $code $output $row.Expected $row.File
      $pushOutcome = if ($output -match 'PUSHDIAG RESULT \S+ outcome=(\S+) elapsed_ms=(\d+)') { "$($Matches[1])@$($Matches[2])ms" } else { $null }
      $attempts += [pscustomobject]@{ run = $run; exitCode = $code; status = $status; pushOutcome = $pushOutcome }
      Write-Host "Run $run/$Runs`: exit=$code result=$status push=$pushOutcome"
    }
    $count = { param($s) @($attempts | Where-Object status -eq $s).Count }
    $result = [pscustomobject]@{
      set = $Set; row = $row.Id; bun = $version; file = $row.File; pattern = $row.Pattern
      expectedTests = $row.Expected; runs = $Runs; attempts = $attempts
    }
    $result | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $OutputDirectory "$Label-$Set-$($row.Id).json")
    $codes = ($attempts | ForEach-Object { $_.exitCode }) -join ','
    $pushes = ($attempts | ForEach-Object { if ($_.pushOutcome) { $_.pushOutcome } else { '-' } }) -join ','
    $line = "| $($row.Id) | $($row.Expected) | $(& $count 'crash') | $(& $count 'timeout') | $(& $count 'stuck') | $(& $count 'late') | $(& $count 'failure') | $(& $count 'filter-mismatch') | $(& $count 'pass') | $codes | $pushes |"
    # Append each row as it finishes so a job timeout still leaves earlier rows.
    $line | Add-Content -Path $summaryPath
    Write-Host $line
    if ((& $count 'pass') -ne $Runs) { $failedRows++ }
    Write-Host '::endgroup::'
  }
} finally {
  $env:WIN_BUN_REPRO = $savedOptIn
  Pop-Location
}
'' | Add-Content -Path $summaryPath
'crash = Bun crash report/panic text or a native crash exit code. timeout = a bun:test per-test timeout. stuck/late come from the logged push diagnostic (stuck: watchdog after 15 s; late: correct but slower than the original 5 s timeout). filter mismatch = exit 0 but a different number of tests ran than the row names.' |
  Add-Content -Path $summaryPath
Get-Content -Raw $summaryPath | Out-File -Append -FilePath $(if ($env:GITHUB_STEP_SUMMARY) { $env:GITHUB_STEP_SUMMARY } else { Join-Path $OutputDirectory 'step-summary.md' })
Write-Host "Bun $version set $Set finished; rows not passing all $Runs runs: $failedRows"
if ($failedRows -gt 0) { exit 1 }
