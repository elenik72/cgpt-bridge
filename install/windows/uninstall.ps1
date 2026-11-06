<#
.SYNOPSIS
  cgpt-bridge - Windows uninstaller. install.ps1 copies it to
  %LOCALAPPDATA%\cgpt-bridge\uninstall.ps1.

.DESCRIPTION
    powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\cgpt-bridge\uninstall.ps1"

  Removes what install.ps1 created and nothing else:
  - stops the gateway and removes its autostart value "cgpt-gateway" under
    HKCU\Software\Microsoft\Windows\CurrentVersion\Run (and Task Manager's
    on/off flag for it; other Run values are not touched) and a cgpt-gateway.lnk of an earlier version from the Startup
    folder;
  - removes the registry key of the native host if it still points at the
    installed manifest;
  - removes bin\ (with hosts an update renamed aside), extension\, the host
    manifest, start-gateway.ps1, uninstall.ps1, gateway.log and the agent
    template
    from %LOCALAPPDATA%\cgpt-bridge (the folder itself only when nothing else
    is left in it);
  - removes the gateway config, sessions, rate-limit state and records of unread answers
    (%APPDATA%\cgpt-gateway);
  - removes provider "cgpt" from the OpenCode config, small_model if it is
    cgpt/local-title, model if it names a cgpt/ model and default_agent if it
    is cgpt and no agent cgpt is left (the previous file is kept as
    opencode.json.bak);
  - removes the OpenCode agents cgpt and cgpt-sub (opencode\agents\cgpt.md
    and cgpt-sub.md), each if it is the one install.ps1 wrote (a copy the
    user edited is kept as <name>.md.bak), and the agents folder if that
    leaves it empty.
#>
param()

$ErrorActionPreference = 'Stop'

$HostName = 'com.cgpt_bridge.host'
$App = Join-Path $env:LOCALAPPDATA 'cgpt-bridge'
$GatewayExe = Join-Path $App 'bin\cgpt-gateway.exe'
$Manifest = Join-Path $App "$HostName.json"
$RegKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
$Startup = [Environment]::GetFolderPath('Startup')
if (-not $Startup) { $Startup = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Startup' }
$LegacyShortcut = Join-Path $Startup 'cgpt-gateway.lnk'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunName = 'cgpt-gateway'
$ApprovedKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
$Gw = Join-Path $env:APPDATA 'cgpt-gateway'
$ConfigHome = if ($env:XDG_CONFIG_HOME) { $env:XDG_CONFIG_HOME } else { Join-Path $env:USERPROFILE '.config' }
$OpenCode = Join-Path $ConfigHome 'opencode\opencode.json'
$Agents = Join-Path $ConfigHome 'opencode\agents'
$status = 0

if ($PSVersionTable.PSVersion.Major -lt 6) { Remove-TypeData -TypeName System.Array -ErrorAction SilentlyContinue }

function Remove-IfPresent([string]$Path) {
  if (Test-Path -LiteralPath $Path) { Remove-Item -LiteralPath $Path -Recurse -Force }
}

# Follow symlinks (dotfiles setups link opencode.json): the edit goes to the
# real file and the link stays a link.
function Resolve-Link([string]$Path) {
  $start = $Path
  $hops = 0
  $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
  while ($null -ne $item -and $item.LinkType -eq 'SymbolicLink') {
    if (++$hops -gt 40) { throw "${start}: more than 40 symbolic links in a row (a cycle?)" }
    $target = @($item.Target)[0]
    if (-not [System.IO.Path]::IsPathRooted($target)) { $target = Join-Path (Split-Path -Parent $item.FullName) $target }
    $Path = [System.IO.Path]::GetFullPath($target)
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
  }
  $Path
}

# A folder that is a link (dotfiles) stays, as with rmdir on macOS.
function Remove-IfEmpty([string]$Dir) {
  $item = Get-Item -LiteralPath $Dir -Force -ErrorAction SilentlyContinue
  if ($null -ne $item -and -not $item.LinkType -and -not (Get-ChildItem -LiteralPath $Dir -Force)) {
    Remove-Item -LiteralPath $Dir -Force
  }
}

Write-Host '==> Gateway'
Get-Process -Name 'cgpt-gateway' -ErrorAction SilentlyContinue |
  Where-Object { $_.Path -eq $GatewayExe } |
  ForEach-Object { Stop-Process -Id $_.Id -Force; $_.WaitForExit(5000) | Out-Null }
if ($null -ne (Get-ItemProperty -Path $RunKey -Name $RunName -ErrorAction SilentlyContinue)) {
  Remove-ItemProperty -Path $RunKey -Name $RunName
}
# Task Manager -> Startup apps keeps its on/off flag for the entry here.
if ($null -ne (Get-ItemProperty -Path $ApprovedKey -Name $RunName -ErrorAction SilentlyContinue)) {
  Remove-ItemProperty -Path $ApprovedKey -Name $RunName
}
Remove-IfPresent $LegacyShortcut

Write-Host '==> Chrome native host'
if (Test-Path -Path $RegKey) {
  if ((Get-Item -Path $RegKey).GetValue('') -eq $Manifest) {
    Remove-Item -Path $RegKey -Force
  } else {
    Write-Host "    $RegKey points at another manifest; left in place"
  }
}

# Remove-Agent <name>: agents\<name>.md, if ours by the marks install.ps1
# checks too; an agent the user wrote stays. A copy of ours the user edited is
# kept as <name>.md.bak.
function Remove-Agent([string]$Name) {
  $agent = Join-Path $Agents "$Name.md"
  Write-Host "==> OpenCode agent ${Name}: $agent"
  try {
    $agentFile = Resolve-Link $agent
    if (Test-Path -LiteralPath $agentFile -PathType Leaf) {
      $text = [System.IO.File]::ReadAllText($agentFile)
      if (($text -cmatch '(?m)^model: cgpt/chatgpt-web[ \t\r]*$') -and ($text -cmatch '(?m)^description: ChatGPT Web \(bridge\)')) {
        $installed = Join-Path $App "opencode-agent-$Name.md"
        $keep = $null
        if (-not (Test-Path -LiteralPath $installed -PathType Leaf)) {
          $keep = "no installed template to compare with: kept as $agent.bak to be safe"
        } elseif ([Convert]::ToBase64String([System.IO.File]::ReadAllBytes($agentFile)) -cne
                  [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($installed))) {
          $keep = "the file differs from the installed one: kept as $agent.bak"
        }
        # A failed copy throws: the file then stays (see the catch below).
        if ($keep) {
          Copy-Item -LiteralPath $agentFile -Destination "$agent.bak" -Force
          Write-Host "    $keep"
        }
        Remove-Item -LiteralPath $agentFile -Force
        if ($agentFile -ne $agent) { Write-Host "    removed $agentFile; the link $agent to it is left in place" }
        Remove-IfEmpty $Agents
      } else {
        Write-Host "    $agent is not ours; left in place"
      }
    }
  } catch {
    Write-Warning "could not remove $agent ($($_.Exception.Message)): agent $Name left in place; remove it by hand"
    $script:status = 1
  }
}
Remove-Agent 'cgpt'
Remove-Agent 'cgpt-sub'
# default_agent "cgpt" is dropped from opencode.json below once no agent file
# cgpt is left (it would name a missing agent), like a model of provider cgpt.
$agentGone = -not (Test-Path -LiteralPath (Join-Path $Agents 'cgpt.md'))

Write-Host "==> OpenCode provider cgpt: $OpenCode"
try {
  $ocFile = Resolve-Link $OpenCode
  if (Test-Path -LiteralPath $ocFile) {
    $text = [System.IO.File]::ReadAllText($ocFile)
    $cfg = if ($text.Trim()) { $text | ConvertFrom-Json } else { $null }
    if ($cfg -is [System.Management.Automation.PSCustomObject]) {
      $before = $cfg | ConvertTo-Json -Depth 64 -Compress
      $provider = $cfg.PSObject.Properties['provider']
      if ($null -ne $provider -and $provider.Value -is [System.Management.Automation.PSCustomObject]) {
        $provider.Value.PSObject.Properties.Remove('cgpt')
        if (@($provider.Value.PSObject.Properties).Count -eq 0) { $cfg.PSObject.Properties.Remove('provider') }
      }
      if ($cfg.small_model -eq 'cgpt/local-title') { $cfg.PSObject.Properties.Remove('small_model') }
      if ($cfg.model -is [string] -and $cfg.model.StartsWith('cgpt/')) { $cfg.PSObject.Properties.Remove('model') }
      # An agent cgpt defined in opencode.json itself is the user's own.
      $ownAgent = $null -ne $cfg.PSObject.Properties['agent'] -and $cfg.agent -is [System.Management.Automation.PSCustomObject] -and
                  $null -ne $cfg.agent.PSObject.Properties['cgpt']
      if ($cfg.default_agent -ceq 'cgpt' -and $agentGone -and -not $ownAgent) { $cfg.PSObject.Properties.Remove('default_agent') }
      if (($cfg | ConvertTo-Json -Depth 64 -Compress) -cne $before) {
        Copy-Item -LiteralPath $ocFile -Destination "$OpenCode.bak" -Force
        $tmp = "$ocFile.tmp"
        [System.IO.File]::WriteAllText($tmp, (($cfg | ConvertTo-Json -Depth 64) + "`n"), (New-Object System.Text.UTF8Encoding($false)))
        [System.IO.File]::Replace($tmp, $ocFile, [NullString]::Value)
        Write-Host "    removed; previous file kept as $OpenCode.bak"
      }
    }
  }
} catch {
  Write-Warning "could not update $OpenCode ($($_.Exception.Message)); remove provider `"cgpt`" from it by hand"
  $status = 1
}

Write-Host "==> Files: $App"
try {
  foreach ($item in 'bin', 'extension', "$HostName.json", 'start-gateway.ps1', 'uninstall.ps1', 'gateway.log', 'opencode-agent-cgpt.md',
                  'opencode-agent-cgpt-sub.md') {
    Remove-IfPresent (Join-Path $App $item)
  }
  Remove-IfEmpty $App
} catch {
  Write-Warning "could not remove all files ($($_.Exception.Message)). Chrome may still run the native host: close Chrome (all windows) and run uninstall.ps1 again."
  $status = 1
}

Write-Host "==> Gateway config: $Gw"
foreach ($item in 'sessions', 'captures', 'config.json', 'cooldown.json', 'unparsed.jsonl', 'unparsed.jsonl.1') { Remove-IfPresent (Join-Path $Gw $item) }
Remove-IfEmpty $Gw

foreach ($dir in $App, $Gw) {
  if (Test-Path -LiteralPath $dir) { Write-Host "    $dir has files that are not ours; left in place" }
}

Write-Host ''
Write-Host 'Done. Remove the cgpt-bridge extension at chrome://extensions yourself.'
exit $status
