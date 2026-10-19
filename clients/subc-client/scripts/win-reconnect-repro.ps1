param(
  [Parameter(Mandatory = $true)][string]$Label,
  [Parameter(Mandatory = $true)][string]$OutputDirectory,
  [int]$Iterations = 200
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$version = (& bun --version).Trim()
if ($LASTEXITCODE -ne 0) { throw 'bun --version failed' }
if ($Label -ne 'latest' -and $version -ne $Label) { throw "Expected Bun $Label, found $version" }
Write-Host "Bun $version; four reconnect experiments; $Iterations iterations each"
$experiments = @(
  @{ Mode = 'same-process'; Arguments = @('same-process') },
  @{ Mode = 'split'; Arguments = @('split') },
  @{ Mode = 'raw-same-process'; Arguments = @('raw', 'same-process') },
  @{ Mode = 'raw-split'; Arguments = @('raw', 'split') }
)
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
Write-Host "Bun $version completed $($experiments.Count) experiments; failures=$failed"
if ($failed -gt 0) { exit 1 }
