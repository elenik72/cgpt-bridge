<#
.SYNOPSIS
  cgpt-bridge - Windows installer for the current user (no admin rights).

.DESCRIPTION
  Run it from the unpacked release bundle (cgpt-bridge-<version>-<build>-windows-x64.zip):

    powershell -ExecutionPolicy Bypass -File .\install.ps1

  1. Copies bin\, extension\, start-gateway.ps1 and uninstall.ps1 to
     %LOCALAPPDATA%\cgpt-bridge\.
  2. Registers the native host with Chrome: the manifest in that folder and the
     key HKCU\Software\Google\Chrome\NativeMessagingHosts\com.cgpt_bridge.host.
  3. Creates the gateway config with a fresh token
     (%APPDATA%\cgpt-gateway\config.json).
  4. Adds provider "cgpt" to the global OpenCode config
     (%USERPROFILE%\.config\opencode\opencode.json); the previous file is kept
     as opencode.json.bak. small_model is set only if there is none.
  5. Adds the OpenCode agent "cgpt"
     (%USERPROFILE%\.config\opencode\agents\cgpt.md): it uses
     cgpt/chatgpt-web and asks before every command, file edit, web access,
     MCP tool and skill, and of all subagents may start only "cgpt-sub"
     (agents\cgpt-sub.md,
     added too), which asks the same way. cgpt is not made the default agent;
     an agent file the user wrote is left alone, an edited copy of ours is
     kept as <name>.md.bak. A failure here only skips that agent.
  6. Registers the gateway for autostart at login: the value "cgpt-gateway"
     under HKCU\Software\Microsoft\Windows\CurrentVersion\Run, which runs
     start-gateway.ps1 (a registry value holds any Unicode path; a .lnk made
     through WScript.Shell cannot). A cgpt-gateway.lnk of an earlier version
     is removed from the Startup folder.
  7. Stops the running gateway, replaces it, starts it and checks that it
     answers.

  Everything is prepared first; the running gateway is stopped, replaced and
  started only at the very end, so a failed run leaves it alone. Chrome may
  stay open: a native host it still runs is renamed aside, not overwritten.
  Running it again updates the files and changes nothing else.
  Exit status 1 when the gateway does not answer after the start.

.PARAMETER NoStart
  Do not start the gateway now (the autostart value is still written).

.PARAMETER NoAgent
  Do not add the OpenCode agents cgpt and cgpt-sub (ones added by an earlier
  run stay as they are).
#>
param([switch]$NoStart, [switch]$NoAgent)

$ErrorActionPreference = 'Stop'

# Chrome derives the extension id from the `key` in extension\manifest.json,
# so it is the same on every machine.
$ExtId = 'oplkebjcjmifidmnbehpadfakodjjoge'
$HostName = 'com.cgpt_bridge.host'

$Src = $PSScriptRoot
$App = Join-Path $env:LOCALAPPDATA 'cgpt-bridge'
$Bin = Join-Path $App 'bin'
$GatewayExe = Join-Path $Bin 'cgpt-gateway.exe'
$HostExe = Join-Path $Bin 'cgpt-bridge-host.exe'
# Staged copies. The gateway one is run (--print-config), so it keeps the .exe
# extension: PowerShell runs only files whose extension is in PATHEXT.
$GatewayNew = Join-Path $Bin 'cgpt-gateway.new.exe'
$HostNew = "$HostExe.new"
$Manifest = Join-Path $App "$HostName.json"
$RegKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
# Autostart: our own value under Run. Earlier versions put cgpt-gateway.lnk
# into the Startup folder; it is removed so the gateway does not start twice.
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunName = 'cgpt-gateway'
$Startup = [Environment]::GetFolderPath('Startup')
if (-not $Startup) { $Startup = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Startup' }
$LegacyShortcut = Join-Path $Startup 'cgpt-gateway.lnk'
$Launcher = Join-Path $App 'start-gateway.ps1'
$Log = Join-Path $App 'gateway.log'
$GatewayConfig = Join-Path $env:APPDATA 'cgpt-gateway\config.json'
$ConfigHome = if ($env:XDG_CONFIG_HOME) { $env:XDG_CONFIG_HOME } else { Join-Path $env:USERPROFILE '.config' }
$OpenCode = Join-Path $ConfigHome 'opencode\opencode.json'
$Agents = Join-Path $ConfigHome 'opencode\agents'

foreach ($f in 'bin\cgpt-gateway.exe', 'bin\cgpt-bridge-host.exe', 'extension\manifest.json',
               'start-gateway.ps1', 'uninstall.ps1', 'opencode-provider.json', 'opencode-agent-cgpt.md',
               'opencode-agent-cgpt-sub.md') {
  if (-not (Test-Path -LiteralPath (Join-Path $Src $f))) {
    throw "$Src\$f not found - run install.ps1 from the unpacked release bundle"
  }
}

# Windows PowerShell 5.1 serializes arrays as {"value":[...],"Count":n}
# while this type data is loaded.
if ($PSVersionTable.PSVersion.Major -lt 6) { Remove-TypeData -TypeName System.Array -ErrorAction SilentlyContinue }

$Utf8 = New-Object System.Text.UTF8Encoding($false)

# Write through a temp file and a rename: an interrupted run never leaves half
# a file. UTF-8 without BOM. Replace keeps the target's permissions.
function Write-TextFile([string]$Path, [string]$Text) {
  $tmp = "$Path.tmp"
  [System.IO.File]::WriteAllText($tmp, $Text, $Utf8)
  if (Test-Path -LiteralPath $Path) { [System.IO.File]::Replace($tmp, $Path, [NullString]::Value) }
  else { [System.IO.File]::Move($tmp, $Path) }
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

# Is <Path> the OpenCode agent this installer writes? Both marks, so an agent
# the user named cgpt for the same model is still theirs.
function Test-OurAgent([string]$Path) {
  $text = [System.IO.File]::ReadAllText($Path, $Utf8)
  ($text -cmatch '(?m)^model: cgpt/chatgpt-web[ \t\r]*$') -and ($text -cmatch '(?m)^description: ChatGPT Web \(bridge\)')
}

# Step 5: OpenCode agent <name>: the bundled opencode-agent-<name>.md, as is
# (UTF-8 without BOM, LF), at agents\<name>.md. OpenCode's own subagents, such
# as general, run every command without asking, so agent cgpt may start only
# cgpt-sub. An agent file is ours when it names the model cgpt/chatgpt-web
# and its description starts with "ChatGPT Web (bridge)" (uninstall.ps1 uses
# the same marks); one the user wrote is not replaced. The install folder
# keeps a copy of the template each agent was written from: a file equal to
# it is unedited and is replaced as is; any other file of ours is an edit and
# is kept as <name>.md.bak first (also when there is no such copy). So an
# unedited older template never overwrites a saved edit. Nothing else in
# OpenCode is switched: default_agent stays as it is, the user picks the
# agent with Tab. The agents are optional: when one cannot be written the
# install goes on. Returns $true when the agent file is ours after the step.
function Install-Agent([string]$Name) {
  $agent = Join-Path $Agents "$Name.md"
  $agentSrc = Join-Path $Src "opencode-agent-$Name.md"
  $agentInstalled = Join-Path $App "opencode-agent-$Name.md"
  $ready = $false
  Write-Host "==> OpenCode agent ${Name}: $agent"
  if ($NoAgent) {
    Write-Host '    not added (-NoAgent)'
    # An agent of an earlier run stays, and stays usable.
    return ((Test-Path -LiteralPath $agent -PathType Leaf) -and (Test-OurAgent $agent))
  }
  try { $agentFile = Resolve-Link $agent } catch { $agentFile = $null }
  if ($null -eq $agentFile) {
    Write-Host "    not added: $agent is a symlink loop"
  } elseif (Test-Path -LiteralPath $agentFile -PathType Container) {
    Write-Host "    not added: $agent is a folder; left as it is"
  } elseif ((Test-Path -LiteralPath $agentFile -PathType Leaf) -and -not (Test-OurAgent $agentFile)) {
    Write-Host "    not added: $agent is not ours (another agent named $Name); left as it is"
  } elseif ((Test-Path -LiteralPath $agentFile -PathType Leaf) -and
            [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($agentFile)) -ceq
            [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($agentSrc))) {
    Write-Host "    agent ${Name}: unchanged"
    $ready = $true
  } else {
    $existed = Test-Path -LiteralPath $agentFile -PathType Leaf
    $unedited = $existed -and (Test-Path -LiteralPath $agentInstalled -PathType Leaf) -and
                [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($agentFile)) -ceq
                [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($agentInstalled))
    try {
      New-Item -ItemType Directory -Force -Path (Split-Path -Parent $agentFile) | Out-Null
      if ($existed -and -not $unedited) { Copy-Item -LiteralPath $agentFile -Destination "$agent.bak" -Force }
      Write-TextFile $agentFile ([System.IO.File]::ReadAllText($agentSrc, $Utf8))
      $ready = $true
      if (-not $existed) { Write-Host "    agent ${Name}: added" }
      elseif ($unedited) { Write-Host "    agent ${Name}: updated" }
      else { Write-Host "    agent ${Name}: updated (previous saved as $Name.md.bak)" }
    } catch {
      Remove-Item -LiteralPath "$agentFile.tmp" -Force -ErrorAction SilentlyContinue
      Write-Warning "could not write $agent ($($_.Exception.Message)): agent $Name not added; the rest of the install goes on"
    }
  }
  # The template the agent file now comes from, for the next run and for
  # uninstall.ps1. Left as it is when the agent was not (re)written.
  if ($ready) {
    Copy-Item -LiteralPath $agentSrc -Destination $agentInstalled -Force -ErrorAction SilentlyContinue
  }
  return $ready
}

function Get-Gateway {
  @(Get-Process -Name 'cgpt-gateway' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $GatewayExe })
}

function Stop-Gateway {
  Get-Gateway | ForEach-Object { Stop-Process -Id $_.Id -Force; $_.WaitForExit(5000) | Out-Null }
}

# Put a staged file in place of its target. A running exe (the host while
# Chrome is open) cannot be overwritten but can be renamed: the old file is
# moved aside and deleted as soon as nothing uses it (here, or on a later run).
function Install-Staged([string]$Staged, [string]$Target) {
  if (Test-Path -LiteralPath $Target) {
    $aside = "$Target.$([guid]::NewGuid().ToString('N').Substring(0, 8)).old"
    Move-Item -LiteralPath $Target -Destination $aside
    Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue
  }
  Move-Item -LiteralPath $Staged -Destination $Target
}

# HTTP status of GET /v1/models with the token, 0 when nothing answers.
function Get-GatewayStatus([int]$Port, [string]$Token) {
  $options = @{}
  if ($PSVersionTable.PSVersion.Major -ge 7) { $options.NoProxy = $true }
  try {
    $r = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/v1/models" `
      -Headers @{ Authorization = "Bearer $Token" } -TimeoutSec 1 @options
    return [int]$r.StatusCode
  } catch {
    $response = $_.Exception.Response
    if ($null -ne $response) { return [int]$response.StatusCode }
    return 0
  }
}

# What the Run value (and the installer's own start) runs. conhost --headless
# (Windows 10 1809 and later) gives powershell no console window at all; the
# only path in the line is the launcher's, in double quotes (a Windows path
# cannot contain one). A Run value may be 260 characters at most.
$RunExe = 'conhost.exe'
$RunArgs = "--headless powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$Launcher`""
$RunData = "$RunExe $RunArgs"

$status = 0
$agentReady = $false
try {
  # -------------------------------------------------------------------------
  # 1. Files. The gateway binary is only staged here; it replaces the running
  #    one in step 7.
  # -------------------------------------------------------------------------
  Write-Host "==> Files: $App"
  New-Item -ItemType Directory -Force -Path $Bin | Out-Null
  # Hosts renamed aside by an earlier run, deleted once Chrome let them go.
  Get-ChildItem -LiteralPath $Bin -Filter '*.old' -Force | Remove-Item -Force -ErrorAction SilentlyContinue
  Copy-Item -LiteralPath (Join-Path $Src 'bin\cgpt-bridge-host.exe') -Destination $HostNew -Force
  Copy-Item -LiteralPath (Join-Path $Src 'bin\cgpt-gateway.exe') -Destination $GatewayNew -Force
  # Files from a downloaded zip carry the "from the internet" mark; these are
  # the user's own copies now.
  Unblock-File -LiteralPath $HostNew, $GatewayNew
  Install-Staged $HostNew $HostExe
  $ext = Join-Path $App 'extension'
  if (Test-Path -LiteralPath $ext) { Remove-Item -LiteralPath $ext -Recurse -Force }
  Copy-Item -LiteralPath (Join-Path $Src 'extension') -Destination $ext -Recurse
  Copy-Item -LiteralPath (Join-Path $Src 'uninstall.ps1') -Destination (Join-Path $App 'uninstall.ps1') -Force
  Copy-Item -LiteralPath (Join-Path $Src 'start-gateway.ps1') -Destination $Launcher -Force
  Get-ChildItem -LiteralPath $ext -Recurse -File | Unblock-File
  Unblock-File -LiteralPath (Join-Path $App 'uninstall.ps1'), $Launcher

  # -------------------------------------------------------------------------
  # 2. Native host: manifest next to the binaries, registry key pointing at it.
  # -------------------------------------------------------------------------
  Write-Host "==> Chrome native host: $RegKey"
  $hostJson = $HostExe.Replace('\', '\\')
  Write-TextFile $Manifest @"
{
  "name": "$HostName",
  "description": "cgpt-bridge native messaging host",
  "path": "$hostJson",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://$ExtId/"
  ]
}
"@
  if (-not (Test-Path -Path $RegKey)) { New-Item -Path $RegKey -Force | Out-Null }
  Set-Item -Path $RegKey -Value $Manifest

  # -------------------------------------------------------------------------
  # 3. Gateway config: the gateway creates it (with a random token) on first
  #    use and prints port and token. The token is not shown.
  # -------------------------------------------------------------------------
  Write-Host '==> Gateway config'
  $printed = & $GatewayNew --print-config
  if ($LASTEXITCODE -ne 0) { throw "cgpt-gateway --print-config failed (exit $LASTEXITCODE)" }
  $gw = ($printed | Out-String) | ConvertFrom-Json
  $port = [int]$gw.port
  $token = [string]$gw.token

  # -------------------------------------------------------------------------
  # 4. OpenCode provider "cgpt"; small_model only if there is none; the rest
  #    of the file is kept. The file is rewritten only when something changes.
  # -------------------------------------------------------------------------
  Write-Host "==> OpenCode provider cgpt: $OpenCode"
  $provider = [System.IO.File]::ReadAllText((Join-Path $Src 'opencode-provider.json')) | ConvertFrom-Json
  $provider.options.baseURL = "http://127.0.0.1:$port/v1"
  $provider.options.apiKey = $token

  $ocFile = Resolve-Link $OpenCode
  $exists = Test-Path -LiteralPath $ocFile
  $cfg = [pscustomobject]@{ '$schema' = 'https://opencode.ai/config.json' }
  if ($exists) {
    $text = [System.IO.File]::ReadAllText($ocFile)
    $cfg = [pscustomobject]@{}
    if ($text.Trim()) {
      try { $cfg = $text | ConvertFrom-Json }
      catch { throw "$OpenCode is not plain JSON ($($_.Exception.Message)). Nothing was changed and the running gateway was left alone; add provider cgpt by hand (README, section Install)." }
    }
  }
  if ($cfg -isnot [System.Management.Automation.PSCustomObject]) { throw "${OpenCode}: the top level is not an object" }
  $before = $cfg | ConvertTo-Json -Depth 64 -Compress

  if ($null -eq $cfg.PSObject.Properties['provider']) {
    $cfg | Add-Member -NotePropertyName provider -NotePropertyValue ([pscustomobject]@{})
  }
  if ($cfg.provider -isnot [System.Management.Automation.PSCustomObject]) { throw "${OpenCode}: `"provider`" is not an object" }
  $existing = $cfg.provider.PSObject.Properties['cgpt']
  if ($null -ne $existing) { $existing.Value = $provider }
  else { $cfg.provider | Add-Member -NotePropertyName cgpt -NotePropertyValue $provider }
  if ($null -eq $cfg.PSObject.Properties['small_model']) {
    $cfg | Add-Member -NotePropertyName small_model -NotePropertyValue 'cgpt/local-title'
  }

  if (-not $exists -or ($cfg | ConvertTo-Json -Depth 64 -Compress) -cne $before) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ocFile) | Out-Null
    if ($exists) {
      Copy-Item -LiteralPath $ocFile -Destination "$OpenCode.bak" -Force
      Write-Host "    previous file kept as $OpenCode.bak"
    }
    Write-TextFile $ocFile (($cfg | ConvertTo-Json -Depth 64) + "`n")
  } else {
    Write-Host '    already up to date'
  }

  # -------------------------------------------------------------------------
  # 5. OpenCode agents cgpt and cgpt-sub (Install-Agent above): cgpt may start
  #    no subagent but cgpt-sub.
  # -------------------------------------------------------------------------
  $subReady = Install-Agent 'cgpt-sub'
  # A cgpt-sub the user wrote has its own permissions, and cgpt may start it.
  if (-not $subReady -and -not $NoAgent -and (Test-Path -LiteralPath (Join-Path $Agents 'cgpt-sub.md'))) {
    Write-Warning "agent cgpt may start cgpt-sub, which is not ours: it runs with its own permissions ($(Join-Path $Agents 'cgpt-sub.md'))"
  }
  $agentReady = Install-Agent 'cgpt'

  # -------------------------------------------------------------------------
  # 6. Autostart: the Run value runs start-gateway.ps1 at login, which starts
  #    the gateway without a console window (log in gateway.log). The script
  #    sets the gateway's working directory itself and takes every path from
  #    its own folder, so nothing else depends on the launcher's directory.
  # -------------------------------------------------------------------------
  Write-Host "==> Autostart: $RunKey\$RunName"
  if ($RunData.Length -gt 260) {
    throw "the autostart command is $($RunData.Length) characters; Windows reads at most 260 from a Run value. The install folder $App is too long: use a shorter profile path."
  }
  if (-not (Test-Path -Path $RunKey)) { New-Item -Path $RunKey -Force | Out-Null }
  Set-ItemProperty -Path $RunKey -Name $RunName -Value $RunData -Type String
  if (Test-Path -LiteralPath $LegacyShortcut) {
    Remove-Item -LiteralPath $LegacyShortcut -Force
    Write-Host "    removed $LegacyShortcut of an earlier version"
  }

  # -------------------------------------------------------------------------
  # 7. Swap: stop the running gateway, put the new binary in place, start it
  #    the way every login will (the Run value's command line, through
  #    ShellExecute like Explorer, so it is not tied to this console window
  #    and inherits none of its handles) and check that it answers with the
  #    token.
  # -------------------------------------------------------------------------
  Write-Host '==> Gateway'
  Stop-Gateway
  Install-Staged $GatewayNew $GatewayExe
  if ($NoStart) {
    Write-Host '    not started (-NoStart)'
  } else {
    Start-Process -FilePath $RunExe -ArgumentList $RunArgs -WindowStyle Hidden
    $code = 0
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    while ($clock.Elapsed.TotalSeconds -lt 10) {
      $code = Get-GatewayStatus $port $token
      if ($code -eq 200) { break }
      Start-Sleep -Milliseconds 500
    }
    if ($code -eq 200) {
      Write-Host "    gateway answers on http://127.0.0.1:$port (log: $Log)"
    } elseif ($code -eq 401) {
      Write-Host "error: port $port answers but rejects the gateway token: another program uses port $port."
      Write-Host "       Free the port or set another `"port`" in $GatewayConfig, then run install.ps1 again."
      $status = 1
    } else {
      Write-Host "error: the gateway does not answer on http://127.0.0.1:$port/v1/models."
      Write-Host "       Is port $port taken by another program? Check: Get-NetTCPConnection -State Listen -LocalPort $port"
      Write-Host "       Gateway log: $Log"
      $status = 1
    }
  }
} finally {
  # Staged files of a failed run.
  foreach ($staged in $HostNew, $GatewayNew) {
    if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
  }
}

Write-Host @"

Done. Next steps:
  1. Chrome -> chrome://extensions -> Developer mode -> Load unpacked -> choose
       $App\extension
     Already loaded from this folder? Press the reload button on the
     cgpt-bridge card: only then does Chrome start the new native host.
     Loaded from anywhere else (a source checkout, an older bundle)? Remove
     that card first.
  2. Open https://chatgpt.com and log in (chat history on). On the first
     OpenCode request the extension opens its own ChatGPT window: leave it
     open, do not type in it, and click once inside its page.
"@
if ($agentReady) {
  Write-Host @"
  3. In OpenCode press Tab to switch to the agent cgpt, or start it with
       opencode --agent cgpt
     It uses cgpt/chatgpt-web and asks before every command, file edit, web
     access, MCP tool and skill; of the subagents it may start only cgpt-sub,
     which asks the
     same way. It is not made the default agent: OpenCode starts with your
     usual agent and model, and you pick cgpt with Tab. If you choose the
     model cgpt/chatgpt-web in another agent (/models), nothing asks.
"@
} else {
  Write-Host @"
  3. In OpenCode choose the model cgpt/chatgpt-web (/models), or set
       "model": "cgpt/chatgpt-web"  in $OpenCode
"@
}
Write-Host @"
Warning: when OpenCode starts, it sends the unfinished turns of old sessions
(for example of a killed opencode run) to ChatGPT again. Delete the sessions
you do not want to go on (/sessions in the TUI); see the README.
Uninstall: powershell -ExecutionPolicy Bypass -File "$App\uninstall.ps1"
"@
exit $status
