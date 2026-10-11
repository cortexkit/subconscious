$ErrorActionPreference = 'Stop'
$engine = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
$probe = 'try { Import-Module Microsoft.PowerShell.Security -ErrorAction Stop; [pscustomobject]@{ edition = $PSVersionTable.PSEdition; version = $PSVersionTable.PSVersion.ToString(); modulePath = $env:PSModulePath; securityModule = $true } | ConvertTo-Json -Compress } catch { [pscustomobject]@{ edition = $PSVersionTable.PSEdition; version = $PSVersionTable.PSVersion.ToString(); modulePath = $env:PSModulePath; securityModule = $false; error = $_.Exception.Message } | ConvertTo-Json -Compress }'
$original = $env:PSModulePath
New-Item -ItemType Directory -Force private-files-evidence | Out-Null
try {
  & $engine -NoProfile -NonInteractive -Command $probe | Tee-Object -FilePath private-files-evidence/powershell-inherited-module-path.json
  if ($LASTEXITCODE) { throw 'PowerShell inherited-path diagnostic could not launch' }
  $env:PSModulePath = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/Modules'
  & $engine -NoProfile -NonInteractive -Command $probe | Tee-Object -FilePath private-files-evidence/powershell-clean-module-path.json
  if ($LASTEXITCODE) { throw 'PowerShell clean-path diagnostic could not launch' }
  $clean = Get-Content -Raw private-files-evidence/powershell-clean-module-path.json | ConvertFrom-Json
  if (!$clean.securityModule) { throw 'Windows PowerShell system module unavailable even with a clean module path' }
} finally { $env:PSModulePath = $original }
