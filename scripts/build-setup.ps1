<#
.SYNOPSIS
  cgpt-bridge - builds the Windows setup (.exe) from an unpacked release bundle.

.DESCRIPTION
  Wraps the bundle that scripts/build-release.ps1 makes (the staged folder
  dist\cgpt-bridge-<version>-<build>-windows-x64, or that zip unpacked) into
  one setup file with Inno Setup 6 (install/windows/cgpt-bridge.iss):

    pwsh ./scripts/build-setup.ps1 -Bundle dist\cgpt-bridge-<version>-<build>-windows-x64

  Output: <OutDir>\cgpt-bridge-<version>-<build>-windows-x64-setup.exe, and a
  line for it appended to <OutDir>\SHA256SUMS when that file exists (the
  bundle's line stays first).

  ISCC.exe is taken from -Iscc, else from PATH, else from Inno Setup 6's
  usual install folders.
#>
param(
  [Parameter(Mandatory = $true)][string]$Bundle,
  [string]$OutDir = 'dist',
  [string]$Iscc
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Bundle = (Resolve-Path -LiteralPath $Bundle).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

foreach ($f in 'install.ps1', 'VERSION', 'bin\cgpt-gateway.exe', 'extension\manifest.json') {
  if (-not (Test-Path -LiteralPath (Join-Path $Bundle $f))) { throw "$Bundle\$f not found: not an unpacked Windows bundle" }
}

if (-not $Iscc) {
  $cmd = Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue
  if ($cmd) { $Iscc = $cmd.Source }
}
if (-not $Iscc) {
  $Iscc = @(
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
  ) | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -First 1
}
if (-not $Iscc) { throw 'ISCC.exe (Inno Setup 6) not found: install Inno Setup 6 or pass -Iscc' }

$version = (Get-Content -LiteralPath (Join-Path $Bundle 'VERSION') -Raw).Trim()
$outName = (Split-Path -Leaf $Bundle) + '-setup'
$iss = Join-Path $Root 'install\windows\cgpt-bridge.iss'

Write-Host "==> $Iscc"
& $Iscc /Qp "/DBundleDir=$Bundle" "/DAppVersion=$version" "/DOutName=$outName" "/O$OutDir" $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit $LASTEXITCODE)" }

$exe = Join-Path $OutDir "$outName.exe"
if (-not (Test-Path -LiteralPath $exe)) { throw "$exe was not made" }
$sums = Join-Path $OutDir 'SHA256SUMS'
if (Test-Path -LiteralPath $sums) {
  # Same format as the bundle's line: lowercase hex, two spaces, name, LF.
  $hash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
  [System.IO.File]::AppendAllText($sums, "$hash  $outName.exe`n", (New-Object System.Text.UTF8Encoding($false)))
}

Write-Host ''
Write-Host "Built setup: $exe"
