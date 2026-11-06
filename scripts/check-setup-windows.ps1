<#
.SYNOPSIS
  cgpt-bridge - checks for the Windows setup .exe (run by CI on windows-latest).

.DESCRIPTION
  Installs the setup silently for the account that runs it (the real
  LOCALAPPDATA, APPDATA, profile and HKCU: Inno Setup takes the folders from
  Windows, not from environment variables), checks what install.ps1 must
  have done and the setup's own uninstall entry, runs the setup again (an
  update), uninstalls through that entry and checks that nothing is left.
  Then installs once more without the OpenCode agent and uninstalls.

    ./scripts/check-setup-windows.ps1 -Setup dist\cgpt-bridge-<version>-<build>-windows-x64-setup.exe

  It refuses to run outside GitHub Actions unless -Force is given: it
  installs and removes cgpt-bridge for real.
#>
param(
  [Parameter(Mandatory = $true)][string]$Setup,
  [switch]$Force
)

if (-not $env:GITHUB_ACTIONS -and -not $Force) {
  [Console]::Error.WriteLine('check-setup-windows.ps1 installs and removes cgpt-bridge for this account; it is meant for throwaway CI runners. Pass -Force to run it here anyway.')
  exit 2
}

$ErrorActionPreference = 'Stop'
$Setup = (Resolve-Path -LiteralPath $Setup).Path

$App = Join-Path $env:LOCALAPPDATA 'cgpt-bridge'
$SetupDir = Join-Path $App 'setup'
$GwConfig = Join-Path $env:APPDATA 'cgpt-gateway\config.json'
$ConfigHome = if ($env:XDG_CONFIG_HOME) { $env:XDG_CONFIG_HOME } else { Join-Path $env:USERPROFILE '.config' }
$Agent = Join-Path $ConfigHome 'opencode\agents\cgpt.md'
$Sub = Join-Path $ConfigHome 'opencode\agents\cgpt-sub.md'
$HostKey = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.cgpt_bridge.host'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{6F2A4C1E-8B3D-4E7A-9C51-2D0B7E4A9F13}_is1'
$TempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }

$script:Fails = 0
function Ok($what) { Write-Host "ok   - $what" }
function Fail($what) { Write-Host "FAIL - $what"; $script:Fails++ }
function Check($what, [bool]$cond) { if ($cond) { Ok $what } else { Fail $what } }

function GatewayStatus {
  if (-not (Test-Path -LiteralPath $GwConfig)) { return 0 }
  $cfg = [System.IO.File]::ReadAllText($GwConfig) | ConvertFrom-Json
  try {
    $r = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$($cfg.port)/v1/models" `
      -Headers @{ Authorization = "Bearer $($cfg.token)" } -TimeoutSec 2
    return [int]$r.StatusCode
  } catch {
    if ($null -ne $_.Exception.Response) { return [int]$_.Exception.Response.StatusCode }
    return 0
  }
}

function GatewayRunning {
  @(Get-Process -Name 'cgpt-gateway' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq (Join-Path $App 'bin\cgpt-gateway.exe') }).Count -gt 0
}

function RunSetup([string]$label, [string[]]$extra = @()) {
  $log = Join-Path $TempRoot "cgpt-setup-$label.log"
  $argv = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"") + $extra
  # Not -Wait: in PowerShell 7 it waits for every process the setup started,
  # and the gateway it starts keeps running.
  $p = Start-Process -FilePath $Setup -ArgumentList $argv -PassThru
  $p.WaitForExit()
  Write-Host "    setup ($label) exit code $($p.ExitCode), log $log"
  if ($p.ExitCode -ne 0) {
    if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log -Tail 40 | ForEach-Object { Write-Host "    | $_" } }
    $installLog = Join-Path $SetupDir 'install.log'
    if (Test-Path -LiteralPath $installLog) { Get-Content -LiteralPath $installLog | ForEach-Object { Write-Host "    > $_" } }
  }
  $p.ExitCode
}

# The uninstaller copies itself to a temp folder, starts that copy and exits
# at once: wait for the uninstall entry to go.
function RunUninstall {
  $entry = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
  if ($null -eq $entry) { Fail 'uninstall entry present before uninstall'; return }
  $exe = ($entry.UninstallString -replace '^"([^"]+)".*$', '$1')
  (Start-Process -FilePath $exe -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -PassThru).WaitForExit()
  $clock = [System.Diagnostics.Stopwatch]::StartNew()
  while ((Test-Path -Path $UninstallKey) -and $clock.Elapsed.TotalSeconds -lt 120) { Start-Sleep -Milliseconds 500 }
  # The temp copy deletes the setup folder last.
  while ((Test-Path -LiteralPath $App) -and $clock.Elapsed.TotalSeconds -lt 120) { Start-Sleep -Milliseconds 500 }
}

function CheckInstalled([string]$label, [bool]$withAgent) {
  Write-Host "--- installed ($label)"
  foreach ($f in 'bin\cgpt-gateway.exe', 'bin\cgpt-bridge-host.exe', 'extension\manifest.json',
                 'uninstall.ps1', 'start-gateway.ps1', 'com.cgpt_bridge.host.json') {
    Check "$f in $App" (Test-Path -LiteralPath (Join-Path $App $f))
  }
  Check 'uninstaller in the setup folder' (@(Get-ChildItem -LiteralPath $SetupDir -Filter 'unins*.exe' -ErrorAction SilentlyContinue).Count -gt 0)
  $log = Join-Path $SetupDir 'install.log'
  Check 'install.log says the gateway answers' ((Test-Path -LiteralPath $log) -and ((Get-Content -LiteralPath $log -Raw) -match 'gateway answers on'))
  Check 'no bundle left in the setup folder' (-not (Test-Path -LiteralPath (Join-Path $SetupDir 'bundle')))
  Check 'native host key points at the manifest' ((Get-Item -Path $HostKey -ErrorAction SilentlyContinue).GetValue('') -eq (Join-Path $App 'com.cgpt_bridge.host.json'))
  Check 'autostart value cgpt-gateway' ($null -ne (Get-ItemProperty -Path $RunKey -Name 'cgpt-gateway' -ErrorAction SilentlyContinue))
  $entry = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
  Check 'uninstall entry "cgpt-bridge" (per user)' ($null -ne $entry -and $entry.DisplayName -eq 'cgpt-bridge')
  Check 'gateway answers /v1/models with the token' ((GatewayStatus) -eq 200)
  if ($withAgent) {
    Check 'OpenCode agent cgpt written' (Test-Path -LiteralPath $Agent)
    Check 'OpenCode subagent cgpt-sub written' (Test-Path -LiteralPath $Sub)
  }
  else {
    Check 'no OpenCode agent cgpt without the task' (-not (Test-Path -LiteralPath $Agent))
    Check 'no OpenCode subagent cgpt-sub without the task' (-not (Test-Path -LiteralPath $Sub))
  }
}

function CheckRemoved([string]$label) {
  Write-Host "--- removed ($label)"
  Check 'uninstall entry gone' (-not (Test-Path -Path $UninstallKey))
  Check "$App gone" (-not (Test-Path -LiteralPath $App))
  Check 'native host key gone' (-not (Test-Path -Path $HostKey))
  Check 'autostart value gone' ($null -eq (Get-ItemProperty -Path $RunKey -Name 'cgpt-gateway' -ErrorAction SilentlyContinue))
  Check 'gateway config gone' (-not (Test-Path -LiteralPath $GwConfig))
  Check 'OpenCode agent cgpt gone' (-not (Test-Path -LiteralPath $Agent))
  Check 'OpenCode subagent cgpt-sub gone' (-not (Test-Path -LiteralPath $Sub))
  Check 'gateway stopped' (-not (GatewayRunning))
}

Write-Host "==> setup: $Setup"
Check 'nothing installed before the check' (-not (Test-Path -Path $UninstallKey) -and -not (Test-Path -LiteralPath $App))

Check 'install exits 0' ((RunSetup 'install') -eq 0)
CheckInstalled 'first install' $true

Check 'second run (update) exits 0' ((RunSetup 'update') -eq 0)
CheckInstalled 'after update' $true

RunUninstall
CheckRemoved 'uninstall'

Check 'install without the agent exits 0' ((RunSetup 'noagent' @('/MERGETASKS="!agent"')) -eq 0)
CheckInstalled 'without the agent' $false
RunUninstall
CheckRemoved 'second uninstall'

Write-Host ''
if ($script:Fails -gt 0) {
  Write-Host "$($script:Fails) setup check(s) failed"
  exit 1
}
Write-Host 'all setup checks passed'
