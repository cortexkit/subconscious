param(
  [Parameter(Mandatory = $true)][string]$Label,
  [Parameter(Mandatory = $true)][string]$OutputDirectory,
  [int]$Iterations = 200
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path $OutputDirectory).Path
$version = (& bun --version).Trim()
if ($LASTEXITCODE -ne 0) { throw 'bun --version failed' }
if ($Label -ne 'latest' -and $version -ne $Label) { throw "Expected Bun $Label, found $version" }
Write-Host "Bun $version; four direct layouts ($Iterations iterations each), two in-runner tests, and five baseline runs"
$experiments = @(
  @{ Mode = 'same-process'; Arguments = @('same-process') },
  @{ Mode = 'split'; Arguments = @('split') },
  @{ Mode = 'raw-same-process'; Arguments = @('raw', 'same-process') },
  @{ Mode = 'raw-split'; Arguments = @('raw', 'split') }
)

function Invoke-TestExperiment([string]$Mode, [string]$File, [int]$Runs) {
  $attempts = @()
  Write-Host "::group::$Label / Bun $version / $Mode"
  Push-Location (Split-Path $PSScriptRoot -Parent)
  try {
    for ($run = 1; $run -le $Runs; $run++) {
      $log = Join-Path $OutputDirectory "$Label-$Mode-run-$run.log"
      Write-Host "Run $run/$Runs`: bun test $File"
      & bun test $File 2>&1 | Tee-Object -FilePath $log | Out-Host
      $code = $LASTEXITCODE
      $output = (Get-Content -Raw $log) -replace '\x1B\[[0-?]*[ -/]*[@-~]', ''
      # Preserve native exits, but separate Bun/OS crashes from ordinary failing
      # assertions. Windows access violations and aborts may leave no panic text.
      $crashed = ($code -in @(134, 139, -1073741819, -1073740791, -1073741571, -1073741795)) -or
        ($output -match 'Bun has crashed|Segmentation fault|panic\([^)]*thread\):')
      $status = if ($crashed) { 'crash' } elseif ($code -ne 0) { 'failure' } else { 'pass' }
      # A missing opt-in must not produce a green diagnostic with two skips.
      if ($Mode -eq 'under-test' -and $status -eq 'pass' -and $output -notmatch '(?m)^\s*2 pass\s*$') {
        $status = 'failure'
        Write-Host 'Expected both in-runner diagnostic tests to pass, not be skipped.'
      }
      $attempts += [pscustomobject]@{ run = $run; exitCode = $code; status = $status }
      Write-Host "Run $run/$Runs`: exit=$code result=$status"
    }
  } finally {
    Pop-Location
  }
  $crashes = @($attempts | Where-Object status -eq 'crash').Count
  $failures = @($attempts | Where-Object status -eq 'failure').Count
  $status = if ($crashes -gt 0) { 'crash' } elseif ($failures -gt 0) { 'failure' } else { 'pass' }
  [pscustomobject]@{
    mode = $Mode; bun = $version; label = $Label; status = $status
    runs = $Runs; crashes = $crashes; failures = $failures; attempts = $attempts
    processes = @($attempts | ForEach-Object {
      [pscustomobject]@{ role = "bun-test-run-$($_.run)"; exitCode = $_.exitCode; signal = $null; forced = $false }
    })
  } | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $OutputDirectory "$Label-$Mode.json")
  Write-Host "$Mode`: crashes=$crashes/$Runs ordinary-failures=$failures/$Runs"
  Write-Host '::endgroup::'
  return [int]($status -ne 'pass')
}

$failed = 0
foreach ($experiment in $experiments) {
  $stem = Join-Path $OutputDirectory "$Label-$($experiment.Mode)"
  $resultPath = "$stem.json"
  $arguments = $experiment.Arguments
  Write-Host "::group::$Label / Bun $version / $($experiment.Mode)"
  & bun "$PSScriptRoot/win-reconnect-repro.ts" @arguments --iterations $Iterations --result $resultPath 2>&1 |
    Tee-Object -FilePath "$stem.log"
  $observerExitCode = $LASTEXITCODE
  try {
    if (-not (Test-Path $resultPath)) { throw 'Observer produced no JSON' }
    $result = Get-Content -Raw $resultPath | ConvertFrom-Json
  } catch {
    # The shell survives even an observer crash; do not silently omit that row.
    $result = [pscustomobject]@{
      mode = $experiment.Mode
      bun = $version
      iterations = $Iterations
      status = 'observer-failed'
      failure = "Observer produced no readable JSON; inspect its log. $_"
      processes = @()
    }
  }
  $result | Add-Member -NotePropertyName observerExitCode -NotePropertyValue $observerExitCode -Force
  $result | Add-Member -NotePropertyName label -NotePropertyValue $Label -Force
  $result | ConvertTo-Json -Depth 10 | Set-Content -Path $resultPath
  Write-Host "Observer exit=$observerExitCode; result=$($result.status)"
  Write-Host '::endgroup::'
  if ($observerExitCode -ne 0 -or $result.status -ne 'pass') { $failed++ }
}
$savedOptIn = $env:WIN_BUN_REPRO
try {
  $env:WIN_BUN_REPRO = '1'
  $failed += Invoke-TestExperiment 'under-test' 'scripts/win-reconnect-under-test.test.ts' 1
} finally {
  $env:WIN_BUN_REPRO = $savedOptIn
}
$failed += Invoke-TestExperiment 'managed-baseline-5x' 'tests/managed-client.test.ts' 5
Write-Host "Bun $version completed $($experiments.Count + 2) experiments; failed rows=$failed"
if ($failed -gt 0) { exit 1 }
