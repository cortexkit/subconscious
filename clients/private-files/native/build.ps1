param([string]$NodeVersion = (node -p 'process.versions.node'), [string]$Arch = (node -p 'process.arch'))
$ErrorActionPreference = 'Stop'
if ($Arch -notin @('x64', 'arm64')) { throw "Unsupported architecture $Arch" }
$cache = Join-Path $env:TEMP "private-files-node-$NodeVersion-$Arch"
New-Item -ItemType Directory -Force $cache | Out-Null
$base = "https://nodejs.org/dist/v$NodeVersion"
$headers = Join-Path $cache 'headers.tar.gz'
$library = Join-Path $cache 'node.lib'
# nodejs.org downloads sometimes end early on CI runners. Retry them; the
# checksum check below still rejects anything that isn't the published file.
$retry = @{ MaximumRetryCount = 4; RetryIntervalSec = 5 }
Invoke-WebRequest "$base/node-v$NodeVersion-headers.tar.gz" -OutFile $headers @retry
Invoke-WebRequest "$base/win-$Arch/node.lib" -OutFile $library @retry
$shas = (Invoke-WebRequest "$base/SHASUMS256.txt" @retry).Content
foreach ($item in @(@($headers, "node-v$NodeVersion-headers.tar.gz"), @($library, "win-$Arch/node.lib"))) {
  $hash = (Get-FileHash $item[0] -Algorithm SHA256).Hash.ToLower()
  if ($shas -notmatch "(?m)^$hash\s+$([regex]::Escape($item[1]))\s*$") { throw "Node download checksum mismatch: $($item[1])" }
}
& tar -xzf $headers -C $cache
if ($LASTEXITCODE) { throw 'Node headers extraction failed' }
$include = Join-Path $cache "node-v$NodeVersion/include/node"
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products '*' -requiresAny -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 Microsoft.VisualStudio.Component.VC.Tools.ARM64 -property installationPath
if (!$vs) { throw 'Install Visual Studio C++ build tools and the Windows SDK (including ARM64 tools for ARM64).' }
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvarsall.bat'
$target = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
  if ($Arch -eq 'arm64') { 'arm64' } else { 'arm64_amd64' }
} else {
  if ($Arch -eq 'arm64') { 'amd64_arm64' } else { 'amd64' }
}
$out = Join-Path $PSScriptRoot "..\prebuilds\private-files-win32-$Arch.node"
New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null
$obj = Join-Path $cache 'addon.obj'
$hookObject = Join-Path $cache 'delay-load-hook.obj'
$cmd = "`"$vcvars`" $target && cl /nologo /std:c11 /W4 /O2 /MT /TC /c `"$PSScriptRoot\delay-load-hook.c`" /Fo`"$hookObject`" && cl /nologo /std:c11 /W4 /O2 /LD /TC /I`"$include`" `"$PSScriptRoot\addon.c`" /Fo`"$obj`" /link /OUT:`"$out`" /IMPLIB:`"$cache\addon.lib`" `"$hookObject`" `"$library`" /DELAYLOAD:node.exe delayimp.lib advapi32.lib ntdll.lib"
& cmd /d /s /c $cmd
if ($LASTEXITCODE) { throw "MSVC build failed: $LASTEXITCODE" }
Write-Output "N-API v8 prebuild: $out (Node $NodeVersion, $Arch)"
$evidence = Join-Path $PSScriptRoot '../private-files-evidence'
New-Item -ItemType Directory -Force $evidence | Out-Null
$imports = Join-Path $evidence "imports-$Arch.txt"
& cmd /d /s /c "`"$vcvars`" $target && dumpbin /imports `"$out`"" | Tee-Object -FilePath $imports
if ($LASTEXITCODE) { throw 'PE import inspection failed' }
$unit = Join-Path $cache 'open-policy-test.exe'
$unitObject = Join-Path $cache 'open-policy-test.obj'
$unitCommand = "`"$vcvars`" $target && cl /nologo /std:c11 /W4 /MT /TC `"$PSScriptRoot\..\test\native-open-policy.c`" /Fo`"$unitObject`" /Fe`"$unit`""
& cmd /d /s /c $unitCommand
if ($LASTEXITCODE) { throw 'Native open policy unit compilation failed' }
& $unit
if ($LASTEXITCODE) { throw 'Native open policy unit assertions failed' }
$securityUnit = Join-Path $evidence 'native-security.exe'
$securityObject = Join-Path $cache 'native-security.obj'
$securityCommand = "`"$vcvars`" $target && cl /nologo /std:c11 /W4 /MT /TC /I`"$include`" `"$PSScriptRoot\..\test\native-security.c`" /Fo`"$securityObject`" /Fe`"$securityUnit`" /link advapi32.lib ntdll.lib"
& cmd /d /s /c $securityCommand
if ($LASTEXITCODE) { throw 'Native security fault-injection harness compilation failed' }
