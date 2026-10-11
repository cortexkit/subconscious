param([string]$Request)
$ErrorActionPreference = 'Stop'
$diagnostics = @{}
try {
  $data = Get-Content -Raw -LiteralPath $Request | ConvertFrom-Json
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = [Security.Principal.WindowsPrincipal]::new($identity)
  $standard = !$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  $profile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
  $diagnostics = @{ userSid = $identity.User.Value; standardUser = $standard; profile = $profile; profileExists = [IO.Directory]::Exists($profile); environmentProfile = $env:USERPROFILE; workingDirectory = [Environment]::CurrentDirectory }
  $control = [IO.File]::ReadAllText($data.control) -eq 'public-control'
  $diagnostics.controlRead = $control
  if (!$control) { throw 'The standard user could not read the public control file' }
  function IsDenied([scriptblock]$Operation) {
    try { & $Operation | Out-Null; return $false }
    catch {
      $errorObject = $_.Exception
      while ($errorObject.InnerException) { $errorObject = $errorObject.InnerException }
      if ($errorObject -is [UnauthorizedAccessException]) { return $true }
      throw
    }
  }
  $fileDenied = IsDenied { [IO.File]::ReadAllText($data.file) }
  $directoryDenied = IsDenied { [IO.Directory]::GetFileSystemEntries($data.directory) }
  & $data.runtime $data.writerScript $profile
  if ($LASTEXITCODE) { throw "Standard-user native writer/reader test failed (runtime exit $LASTEXITCODE)" }
  @{ standardUser = $standard; controlRead = $control; fileDenied = $fileDenied; directoryDenied = $directoryDenied; standardWriter = $true; diagnostics = $diagnostics } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $data.result
} catch {
  $diagnostics.error = $_.Exception.ToString()
  [Console]::Error.WriteLine(($diagnostics | ConvertTo-Json -Depth 5 -Compress))
  exit 1
}
