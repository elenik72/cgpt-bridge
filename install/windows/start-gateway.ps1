<#
.SYNOPSIS
  cgpt-bridge - starts the gateway without a console window. install.ps1 copies
  it to %LOCALAPPDATA%\cgpt-bridge\start-gateway.ps1; the HKCU Run value
  "cgpt-gateway" runs it at every login, and install.ps1 runs it once to start
  the gateway right away.

.DESCRIPTION
  Every path comes from the folder this script is in, so the Run value holds
  no user path except this script's own (a Run value is limited to 260
  characters). Keep this file ASCII-only: Windows PowerShell 5.1 reads a
  script without a BOM as ANSI.
#>
$App = $PSScriptRoot
Start-Process -FilePath (Join-Path $App 'bin\cgpt-gateway.exe') -WorkingDirectory $App `
  -WindowStyle Hidden -RedirectStandardError (Join-Path $App 'gateway.log')
