<#
.SYNOPSIS
  cgpt-bridge - checks for the Windows installer (run by CI on windows-latest).

.DESCRIPTION
  Runs install.ps1 / uninstall.ps1 of an unpacked bundle in a child
  PowerShell on a temporary profile: LOCALAPPDATA, APPDATA and USERPROFILE
  point into a temp folder. The HKCU registry (native host key, autostart value
  under ...\CurrentVersion\Run) and the Startup folder's location are
  the runner's own (CI machines are throwaway). The installer really starts
  the gateway; the check asks it for /v1/models with the token.

  Usage (from pwsh):
    ./scripts/check-install-windows.ps1 -Bundle <unpacked-bundle-dir> [-Shell powershell|pwsh]

  -Shell picks the PowerShell that runs the installer: Windows PowerShell 5.1
  (powershell, what users have by default) or PowerShell 7 (pwsh).

  The temporary profile lives in a folder named like a real home folder that
  breaks naive scripts: Cyrillic, a space, parentheses and an apostrophe
  (the Russian words "Test Polzovatel" in Cyrillic, then "(O'Brien)").
  Everything the installer writes (host,
  manifest, autostart value, gateway) is under that path, and section F starts the
  installed native host the way Chrome does: cmd.exe /d /s /c "<host>" <origin>.
  This file is ASCII-only on purpose: Windows PowerShell 5.1 reads a script
  without a BOM as ANSI, so the non-ASCII text is built from code points.

  It refuses to run outside GitHub Actions (GITHUB_ACTIONS unset) unless
  -Force is given: the registry keys and values, the Startup folder and the gateway it
  starts are the real ones of the account that runs it.
#>
param(
  [Parameter(Mandatory = $true)][string]$Bundle,
  [ValidateSet('powershell', 'pwsh')][string]$Shell = 'powershell',
  [switch]$Force
)

if (-not $env:GITHUB_ACTIONS -and -not $Force) {
  [Console]::Error.WriteLine(
    'check-install-windows.ps1 writes HKCU\Software\Google\Chrome\NativeMessagingHosts, ' +
    'the HKCU Run key and the Startup folder of this account; it is meant for throwaway CI runners. ' +
    'Pass -Force to run it here anyway.')
  exit 2
}

$ErrorActionPreference = 'Stop'
$Bundle = (Resolve-Path -LiteralPath $Bundle).Path

# RUNNER_TEMP is a long path; %TEMP% on GitHub runners is an 8.3 one
# (RUNNER~1), which would not match the paths Windows reports for processes.
$TempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
# Non-ASCII text from code points, so the file stays ASCII-only.
function U([int[]]$CodePoints) { -join ($CodePoints | ForEach-Object { [char]::ConvertFromUtf32($_) }) }
$WorkRoot = Join-Path $TempRoot ("cgpt-install-check-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$Work = Join-Path $WorkRoot ((U 0x0422, 0x0435, 0x0441, 0x0442, 0x0020, 0x041F, 0x043E, 0x043B, 0x044C, 0x0437, 0x043E, 0x0432, 0x0430, 0x0442, 0x0435, 0x043B, 0x044C) + " (O'Brien)")
$RuText = U 0x041E, 0x0442, 0x0432, 0x0435, 0x0447, 0x0430, 0x0439, 0x0020, 0x043F, 0x043E, 0x002D, 0x0440, 0x0443, 0x0441, 0x0441, 0x043A, 0x0438, 0x0020, 0x1F642
$Saved = @{ LOCALAPPDATA = $env:LOCALAPPDATA; APPDATA = $env:APPDATA; USERPROFILE = $env:USERPROFILE; XDG_CONFIG_HOME = $env:XDG_CONFIG_HOME }
$env:LOCALAPPDATA = Join-Path $Work 'Local'
$env:APPDATA = Join-Path $Work 'Roaming'
$env:USERPROFILE = Join-Path $Work 'profile'
$env:XDG_CONFIG_HOME = $null
New-Item -ItemType Directory -Force -Path $env:LOCALAPPDATA, $env:APPDATA, $env:USERPROFILE | Out-Null

$App = Join-Path $env:LOCALAPPDATA 'cgpt-bridge'
$Gw = Join-Path $env:APPDATA 'cgpt-gateway'
$Manifest = Join-Path $App 'com.cgpt_bridge.host.json'
$NmRoot = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts'
$RegKey = "$NmRoot\com.cgpt_bridge.host"
$OtherKey = "$NmRoot\com.other.host"
$Startup = [Environment]::GetFolderPath('Startup')
if (-not $Startup) { $Startup = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Startup' }
# Autostart is the value cgpt-gateway under Run; an earlier version wrote
# cgpt-gateway.lnk into Startup (a dummy file stands in for it here).
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunName = 'cgpt-gateway'
$OtherRunName = 'cgpt-check-other'
$LegacyShortcut = Join-Path $Startup 'cgpt-gateway.lnk'
$OtherShortcut = Join-Path $Startup 'cgpt-check-other.lnk'
$ApprovedKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
$OcDir = Join-Path $env:USERPROFILE '.config\opencode'
$Oc = Join-Path $OcDir 'opencode.json'
$AgentDir = Join-Path $OcDir 'agents'
$Agent = Join-Path $AgentDir 'cgpt.md'
$Sub = Join-Path $AgentDir 'cgpt-sub.md'
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$GatewayExe = Join-Path $App 'bin\cgpt-gateway.exe'
$Launcher = Join-Path $App 'start-gateway.ps1'

$script:Fails = 0
function Ok($what) { Write-Host "ok   - $what" }
function Fail($what) { Write-Host "FAIL - $what"; $script:Fails++ }
function Check($what, [bool]$cond) { if ($cond) { Ok $what } else { Fail $what } }
function Eq($what, $actual, $expected) {
  if ("$actual" -ceq "$expected") { Ok $what } else { Fail "${what}: got '$actual', want '$expected'" }
}
function RunValue([string]$name) {
  $p = Get-ItemProperty -Path $RunKey -Name $name -ErrorAction SilentlyContinue
  if ($null -eq $p) { $null } else { $p.$name }
}
# The command line the installer must register, written out independently.
function ExpectedRunValue {
  'conhost.exe --headless powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' + $Launcher + '"'
}
function ReadJson($path) { [System.IO.File]::ReadAllText($path) | ConvertFrom-Json }
# -InputObject: an array stays an array (a pipeline would unroll it).
function Compact($obj) { if ($null -eq $obj) { '' } else { ConvertTo-Json -InputObject $obj -Depth 32 -Compress } }
function Has($obj, $name) { $null -ne $obj -and $null -ne $obj.PSObject.Properties[$name] }
function FileHashOf($path) { if (Test-Path -LiteralPath $path -PathType Leaf) { (Get-FileHash -LiteralPath $path).Hash } else { 'missing' } }
# The line the installer prints right after "==> OpenCode agent cgpt: ..."
# (and after "==> OpenCode agent cgpt-sub: ...").
function AgentLine([string]$out) { if ($out -match '==> OpenCode agent cgpt: [^\n]*\n([^\r\n]*)') { $Matches[1] } else { '' } }
function SubLine([string]$out) { if ($out -match '==> OpenCode agent cgpt-sub: [^\n]*\n([^\r\n]*)') { $Matches[1] } else { '' } }
function Run($file, [string[]]$extra = @()) {
  $out = & $Shell -NoProfile -ExecutionPolicy Bypass -File $file @extra 2>&1 | Out-String
  return @{ Code = $LASTEXITCODE; Out = $out }
}
function Install([string[]]$extra = @()) { Run (Join-Path $Bundle 'install.ps1') $extra }
function Uninstall { Run (Join-Path $App 'uninstall.ps1') }
# Hashes of every file the installer owns; gateway.log is rewritten by each start.
function Snapshot {
  Get-ChildItem -LiteralPath $App, $Gw, $OcDir -Recurse -File -Force |
    Where-Object { $_.Name -ne 'gateway.log' } |
    ForEach-Object { "$($_.FullName) $((Get-FileHash -LiteralPath $_.FullName).Hash)" } |
    Sort-Object | Out-String
}
function GatewayProcesses {
  @(Get-Process -Name 'cgpt-gateway' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $GatewayExe })
}
# Starts the installed host the way Chrome does on Windows: through cmd.exe,
# stdin/stdout/stderr piped, the extension origin as the argument. Chrome
# wraps the whole command in one more pair of quotes, which /s strips.
function StartChromeHost([string]$exe) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $env:ComSpec
  $psi.Arguments = '/d /s /c ""' + $exe + '" chrome-extension://' + $ExtId + '/ --parent-window=0"'
  $psi.UseShellExecute = $false
  $psi.RedirectStandardInput = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  @{ Proc = [System.Diagnostics.Process]::Start($psi); Log = ''; Pending = $null }
}
# Reads the host's stderr until the text read so far matches $pattern.
function WaitHostLog($h, [string]$pattern, [int]$timeoutMs = 10000) {
  $clock = [System.Diagnostics.Stopwatch]::StartNew()
  while ($h.Log -notmatch $pattern) {
    if ($null -eq $h.Pending) { $h.Pending = $h.Proc.StandardError.ReadLineAsync() }
    $left = [int]($timeoutMs - $clock.ElapsedMilliseconds)
    if ($left -le 0 -or -not $h.Pending.Wait($left)) { return $false }
    $line = $h.Pending.Result
    $h.Pending = $null
    if ($null -eq $line) { return $false }
    $h.Log += $line + "`n"
  }
  return $true
}
function Models($port, $token) {
  for ($i = 0; $i -lt 40; $i++) {
    try {
      return Invoke-RestMethod -Uri "http://127.0.0.1:$port/v1/models" -Headers @{ Authorization = "Bearer $token" } -TimeoutSec 2
    } catch { Start-Sleep -Milliseconds 250 }
  }
  return $null
}

# The pinned id must be the one Chrome derives from the manifest `key`.
$key = (ReadJson (Join-Path $Bundle 'extension\manifest.json')).key
$sha = [System.Security.Cryptography.SHA256]::Create().ComputeHash([Convert]::FromBase64String($key))
$ExtId = -join ($sha[0..15] | ForEach-Object { '{0:x2}' -f $_ } | ForEach-Object { $_.ToCharArray() } |
    ForEach-Object { [char]([int][char]'a' + [Convert]::ToInt32([string]$_, 16)) })

try {
  if (-not (Test-Path -LiteralPath (Join-Path $Bundle 'install.ps1'))) {
    Fail "$Bundle\install.ps1 is missing"
    exit 1
  }
  Write-Host "# installer shell: $Shell"
  # "powershell" must really be Windows PowerShell 5.1 and "pwsh" PowerShell 7+.
  $shellVersion = [version]((& $Shell -NoProfile -Command '$v = $PSVersionTable.PSVersion; [string]$v.Major + [char]46 + $v.Minor' | Out-String).Trim())
  Write-Host "# installer shell version: $shellVersion"
  Check "$Shell is the expected PowerShell" $(if ($Shell -eq 'pwsh') { $shellVersion.Major -ge 7 } else { $shellVersion.Major -eq 5 -and $shellVersion.Minor -eq 1 })

  # Work on a copy marked the way a browser download is marked.
  Copy-Item -LiteralPath $Bundle -Destination (Join-Path $Work 'bundle') -Recurse
  $Bundle = Join-Path $Work 'bundle'
  $AgentSrc = Join-Path $Bundle 'opencode-agent-cgpt.md'
  $SubSrc = Join-Path $Bundle 'opencode-agent-cgpt-sub.md'
  Set-Content -LiteralPath (Join-Path $Bundle 'bin\cgpt-gateway.exe') -Stream Zone.Identifier -Value "[ZoneTransfer]`r`nZoneId=3"

  # -------------------------------------------------------------------------
  Write-Host '# A: existing opencode.json with another provider'
  # -------------------------------------------------------------------------
  New-Item -ItemType Directory -Force -Path $OcDir | Out-Null
  $original = @'
{
  "$schema": "https://opencode.ai/config.json",
  "theme": "dark",
  "model": "anthropic/claude-sonnet-4-5",
  "provider": {
    "anthropic": { "options": { "apiKey": "sk-ant-foreign" } }
  },
  "instructions": ["__RU__"]
}
'@
  $original = $original.Replace('__RU__', $RuText)
  [System.IO.File]::WriteAllText($Oc, $original, (New-Object System.Text.UTF8Encoding($false)))
  New-Item -Path $OtherKey -Force | Out-Null
  Set-Item -Path $OtherKey -Value 'C:\other\host.json'
  # Foreign autostart entries: a Run value and a file in Startup. Plain file
  # writes, no WScript.Shell (it cannot save a non-ANSI path). The dummy
  # cgpt-gateway.lnk is what an earlier installer version left behind.
  if (-not (Test-Path -Path $RunKey)) { New-Item -Path $RunKey -Force | Out-Null }
  Set-ItemProperty -Path $RunKey -Name $OtherRunName -Value "$env:SystemRoot\notepad.exe" -Type String
  New-Item -ItemType Directory -Force -Path $Startup | Out-Null
  Set-Content -LiteralPath $OtherShortcut -Value 'not ours'
  Set-Content -LiteralPath $LegacyShortcut -Value 'an old shortcut'

  $r = Install
  Eq 'install.ps1 exits 0' $r.Code 0
  if ($r.Code -ne 0) { Write-Host $r.Out; exit 1 }

  foreach ($f in 'bin\cgpt-gateway.exe', 'bin\cgpt-bridge-host.exe', 'extension\manifest.json', 'uninstall.ps1') {
    Check "$f installed" (Test-Path -LiteralPath (Join-Path $App $f))
  }
  Check 'installed files carry no download mark' ($null -eq (Get-Item -LiteralPath $GatewayExe -Stream Zone.Identifier -ErrorAction SilentlyContinue))
  Eq 'registry key points at the manifest' (Get-Item -Path $RegKey).GetValue('') $Manifest
  $m = ReadJson $Manifest
  Eq 'host manifest name' $m.name 'com.cgpt_bridge.host'
  Eq 'host manifest path' $m.path (Join-Path $App 'bin\cgpt-bridge-host.exe')
  Eq 'host manifest type' $m.type 'stdio'
  Eq 'host manifest origins' (@($m.allowed_origins) -join ',') "chrome-extension://$ExtId/"

  $cfg = ReadJson (Join-Path $Gw 'config.json')
  $token = $cfg.token
  Check 'gateway token is 64 lowercase hex' ($token -cmatch '^[0-9a-f]{64}$')
  Eq 'gateway port' $cfg.port 8765
  Check 'installer output does not show the token' (-not $r.Out.Contains($token))

  Eq 'autostart Run value is the expected command line' (RunValue $RunName) (ExpectedRunValue)
  Check 'the Run value is a plain string' ((Get-Item -Path $RunKey).GetValueKind($RunName) -eq 'String')
  # Windows reads at most 260 characters of a Run value; the profile path here is long.
  $runLength = (RunValue $RunName).Length
  Check "the Run value is at most 260 characters (is $runLength)" ($runLength -le 260)
  Check 'start-gateway.ps1 is installed' (Test-Path -LiteralPath $Launcher)
  Check 'start-gateway.ps1 is ASCII-only' (@([System.IO.File]::ReadAllBytes($Launcher) | Where-Object { $_ -gt 127 }).Count -eq 0)
  Check 'an old cgpt-gateway.lnk is removed from Startup' (-not (Test-Path -LiteralPath $LegacyShortcut))
  Eq 'a foreign Run value is untouched' (RunValue $OtherRunName) "$env:SystemRoot\notepad.exe"

  Check 'installer reports that the gateway answers' ($r.Out -match 'gateway answers on http://127\.0\.0\.1:8765')
  $models = Models $cfg.port $token
  Check 'gateway runs and lists chatgpt-web' ($null -ne $models -and (@($models.data.id) -contains 'chatgpt-web'))

  $ocJson = ReadJson $Oc
  $orig = $original | ConvertFrom-Json
  Eq 'cgpt npm' $ocJson.provider.cgpt.npm '@ai-sdk/openai-compatible'
  Eq 'cgpt baseURL' $ocJson.provider.cgpt.options.baseURL 'http://127.0.0.1:8765/v1'
  Eq 'cgpt apiKey is the gateway token' $ocJson.provider.cgpt.options.apiKey $token
  Check 'model chatgpt-web' (Has $ocJson.provider.cgpt.models 'chatgpt-web')
  Check 'model local-title' (Has $ocJson.provider.cgpt.models 'local-title')
  Eq 'small_model set' $ocJson.small_model 'cgpt/local-title'
  Eq 'model untouched' $ocJson.model 'anthropic/claude-sonnet-4-5'
  Eq 'other provider untouched' (Compact $ocJson.provider.anthropic) (Compact $orig.provider.anthropic)
  Eq 'other keys untouched' $ocJson.theme 'dark'
  Eq 'array kept as an array' (Compact $ocJson.instructions) (Compact $orig.instructions)
  Eq 'UTF-8 kept' (@($ocJson.instructions)[0]) $RuText
  Eq 'opencode.json.bak is the original' ([System.IO.File]::ReadAllText("$Oc.bak")) $original
  Check 'opencode.json gets no agent key' (-not (Has $ocJson 'agent'))
  Check 'opencode.json gets no default_agent' (-not (Has $ocJson 'default_agent'))

  # Rules match last to first, so where it matters the order is checked:
  # opencode_* is denied before the two MCP resource readers ask again, and
  # in read: the .env rules (both cases) come after "*": allow, with
  # .env.example allowed last. grep asks: it is checked against the search
  # pattern, not the path, so an allowed grep could print a .env file.
  function AgentRuleOrder([string]$name, [string[]]$lines) {
    $mcp = @($lines | Where-Object { $_ -ceq '  "opencode_*": deny' -or $_ -ceq '  opencode_list_mcp_resources: ask' -or $_ -ceq '  opencode_read_mcp_resource: ask' })
    Eq "${name}: opencode_* denied, then the MCP resource readers ask" ($mcp -join '|') `
      '  "opencode_*": deny|  opencode_list_mcp_resources: ask|  opencode_read_mcp_resource: ask'
    $at = [Array]::IndexOf($lines, '  read:')
    $block = if ($at -ge 0) { $lines[$at..([Math]::Min($at + 6, $lines.Count - 1))] } else { @() }
    Eq "${name}: read allowed, .env and .ENV ask, .env.example allowed last" ($block -join '|') `
      '  read:|    "*": allow|    "*.env": ask|    "*.env.*": ask|    "*.ENV": ask|    "*.ENV.*": ask|    "*.env.example": allow'
    Check "${name}: grep asks" ($lines -ccontains '  grep: ask')
  }

  Check 'agent cgpt created' (Test-Path -LiteralPath $Agent -PathType Leaf)
  Eq 'agent file is the bundled template' (FileHashOf $Agent) (FileHashOf $AgentSrc)
  $agentBytes = if (Test-Path -LiteralPath $Agent -PathType Leaf) { [System.IO.File]::ReadAllBytes($Agent) } else { [byte[]]@() }
  $agentLines = $Utf8NoBom.GetString($agentBytes).Split([char]10)
  foreach ($line in 'model: cgpt/chatgpt-web', 'mode: primary', '  bash: ask', '  edit: ask', '  webfetch: ask',
                    '  skill: ask', '  execute: deny', '  "opencode_*": deny', '  task:', '  subagent:', '  question: allow') {
    Check "agent file has '$line'" ($agentLines -ccontains $line)
  }
  # Everything not named below asks (MCP tools have no common name): the
  # catch-all must be the first rule, since rules match last to first.
  $permAt = [Array]::IndexOf($agentLines, 'permission:')
  Eq 'agent cgpt asks for everything by default (first rule)' $(if ($permAt -ge 0 -and $permAt + 1 -lt $agentLines.Count) { $agentLines[$permAt + 1] } else { '' }) '  "*": ask'
  # task (OpenCode before 2.1) and subagent (2.1 and later): deny all, then
  # allow cgpt-sub. Rules match last to first, so the order matters.
  $subRules = @()
  for ($i = 0; $i -lt $agentLines.Count; $i++) {
    if ($agentLines[$i] -cmatch '^  (task|subagent):$') { $subRules += $agentLines[$i..([Math]::Min($i + 2, $agentLines.Count - 1))] }
  }
  Eq 'agent cgpt may start only cgpt-sub, under both permission names' ($subRules -join '|') `
    '  task:|    "*": deny|    cgpt-sub: allow|  subagent:|    "*": deny|    cgpt-sub: allow'
  AgentRuleOrder 'agent cgpt' $agentLines
  Check 'subagent cgpt-sub created' (Test-Path -LiteralPath $Sub -PathType Leaf)
  Eq 'subagent file is the bundled template' (FileHashOf $Sub) (FileHashOf $SubSrc)
  $subBytes = if (Test-Path -LiteralPath $Sub -PathType Leaf) { [System.IO.File]::ReadAllBytes($Sub) } else { [byte[]]@() }
  $subLines = $Utf8NoBom.GetString($subBytes).Split([char]10)
  foreach ($line in 'model: cgpt/chatgpt-web', 'mode: subagent', '  bash: ask', '  shell: ask', '  edit: ask',
                    '  webfetch: ask', '  skill: ask', '  execute: deny', '  "opencode_*": deny', '  task: deny', '  subagent: deny') {
    Check "subagent file has '$line'" ($subLines -ccontains $line)
  }
  $permAt = [Array]::IndexOf($subLines, 'permission:')
  Eq 'subagent cgpt-sub asks for everything by default (first rule)' $(if ($permAt -ge 0 -and $permAt + 1 -lt $subLines.Count) { $subLines[$permAt + 1] } else { '' }) '  "*": ask'
  AgentRuleOrder 'subagent cgpt-sub' $subLines
  Check 'the subagent description is plain YAML and carries our mark' (@($subLines | Where-Object { $_ -cmatch '^description: ChatGPT Web \(bridge\)[^:]+$' }).Count -eq 1)
  Check 'subagent file has LF line ends only' (@($subBytes | Where-Object { $_ -eq 13 }).Count -eq 0)
  Check 'the bundled subagent template has LF line ends only' (@([System.IO.File]::ReadAllBytes($SubSrc) | Where-Object { $_ -eq 13 }).Count -eq 0)
  Check 'installer says the subagent was added' ($r.Out -match 'agent cgpt-sub: added')
  Check 'installer tells that cgpt may start only cgpt-sub' ($r.Out -match 'it may start only cgpt-sub')
  Check "the agent description is plain YAML (no ': ' in the value)" (@($agentLines | Where-Object { $_ -cmatch '^description: [^:]+$' }).Count -eq 1)
  Check 'agent file has LF line ends only' (@($agentBytes | Where-Object { $_ -eq 13 }).Count -eq 0)
  Check 'agent file has no BOM' (-not ($agentBytes.Length -ge 3 -and $agentBytes[0] -eq 0xEF -and $agentBytes[1] -eq 0xBB -and $agentBytes[2] -eq 0xBF))
  Check 'the bundled agent template has LF line ends only' (@([System.IO.File]::ReadAllBytes($AgentSrc) | Where-Object { $_ -eq 13 }).Count -eq 0)
  Check 'installer tells to pick the agent with Tab' ($r.Out -match 'Tab to switch to the agent cgpt')
  Check 'installer says the agent was added' ($r.Out -match 'agent cgpt: added')
  Check 'installer says the agent is not the default' ($r.Out -match 'not made the default agent')

  $before = Snapshot
  $r = Install
  Eq 'second install.ps1 exits 0' $r.Code 0
  Eq 'second install changes nothing' (Snapshot) $before
  Eq 'the Run value is unchanged by a second install' (RunValue $RunName) (ExpectedRunValue)
  Eq 'a second install leaves no cgpt-gateway shortcut' (@(Get-ChildItem -LiteralPath $Startup -Filter 'cgpt-gateway*')).Count 0
  Eq 'a foreign Run value is still untouched' (RunValue $OtherRunName) "$env:SystemRoot\notepad.exe"
  Eq 'one gateway process' (GatewayProcesses).Count 1
  Eq 'opencode.json.bak is still the original' ([System.IO.File]::ReadAllText("$Oc.bak")) $original

  # A failed update must leave the running gateway alone.
  $gwPid = "$((GatewayProcesses).Id)"
  $ocNow = [System.IO.File]::ReadAllText($Oc)
  [System.IO.File]::WriteAllText($Oc, '{ "theme": ')
  $r = Install
  Check 'install.ps1 refuses an opencode.json it cannot parse' ($r.Code -ne 0)
  Eq 'a refused install leaves the gateway running' "$((GatewayProcesses).Id)" $gwPid
  Eq 'a refused install leaves no staged files' (@(Get-ChildItem -LiteralPath (Join-Path $App 'bin') | Where-Object { $_.Name -match '\.new(\.exe)?$' })).Count 0
  [System.IO.File]::WriteAllText($Oc, $ocNow)

  Set-Content -LiteralPath (Join-Path $App 'notes.txt') -Value 'a note of the user'
  # An old shortcut that appeared again (an older bundle was run in between), and
  # the flag Task Manager keeps when the user switches the entry off and on.
  Set-Content -LiteralPath $LegacyShortcut -Value 'an old shortcut'
  if (-not (Test-Path -Path $ApprovedKey)) { New-Item -Path $ApprovedKey -Force | Out-Null }
  New-ItemProperty -Path $ApprovedKey -Name $RunName -PropertyType Binary -Value ([byte[]](2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)) -Force | Out-Null
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  if ($r.Code -ne 0) { Write-Host $r.Out }
  Eq 'uninstall stops the gateway' (GatewayProcesses).Count 0
  foreach ($f in 'bin', 'extension', 'uninstall.ps1', 'gateway.log', 'com.cgpt_bridge.host.json', 'opencode-agent-cgpt.md',
                 'opencode-agent-cgpt-sub.md') {
    Check "uninstall removes $f" (-not (Test-Path -LiteralPath (Join-Path $App $f)))
  }
  Check 'uninstall keeps a foreign file in the install dir' (Test-Path -LiteralPath (Join-Path $App 'notes.txt'))
  Check 'uninstall removes the gateway config' (-not (Test-Path -LiteralPath $Gw))
  Check 'uninstall removes the registry key' (-not (Test-Path -Path $RegKey))
  Check 'uninstall keeps a foreign registry key' (Test-Path -Path $OtherKey)
  Check 'uninstall removes the Run value' ($null -eq (RunValue $RunName))
  Check "uninstall removes Task Manager's flag for the Run value" ($null -eq (Get-ItemProperty -Path $ApprovedKey -Name $RunName -ErrorAction SilentlyContinue))
  Check 'uninstall removes start-gateway.ps1' (-not (Test-Path -LiteralPath $Launcher))
  Check 'uninstall removes a legacy cgpt-gateway.lnk' (-not (Test-Path -LiteralPath $LegacyShortcut))
  Eq 'uninstall keeps a foreign Run value' (RunValue $OtherRunName) "$env:SystemRoot\notepad.exe"
  Check 'uninstall keeps a foreign shortcut' (Test-Path -LiteralPath $OtherShortcut)
  $ocJson = ReadJson $Oc
  Check 'uninstall removes provider cgpt' (-not (Has $ocJson.provider 'cgpt'))
  Check 'uninstall removes small_model it set' (-not (Has $ocJson 'small_model'))
  Eq 'uninstall keeps model' $ocJson.model 'anthropic/claude-sonnet-4-5'
  Eq 'uninstall keeps the other provider' (Compact $ocJson.provider.anthropic) (Compact $orig.provider.anthropic)
  Eq 'uninstall keeps UTF-8' (@($ocJson.instructions)[0]) $RuText
  Check 'uninstall removes agent cgpt' (-not (Test-Path -LiteralPath $Agent))
  Check 'uninstall removes subagent cgpt-sub' (-not (Test-Path -LiteralPath $Sub))
  Check 'uninstall removes the agents folder it left empty' (-not (Test-Path -LiteralPath $AgentDir))
  Remove-Item -LiteralPath $App -Recurse -Force

  # -------------------------------------------------------------------------
  Write-Host '# B: no opencode.json yet; registry key replaced by a dev install'
  # -------------------------------------------------------------------------
  Remove-Item -LiteralPath $OcDir -Recurse -Force
  $r = Install @('-NoStart')
  Eq 'install.ps1 -NoStart exits 0' $r.Code 0
  Eq '-NoStart starts no gateway' (GatewayProcesses).Count 0
  $ocJson = ReadJson $Oc
  Eq 'opencode.json created with schema' ($ocJson.'$schema') 'https://opencode.ai/config.json'
  Eq 'created config has cgpt' $ocJson.provider.cgpt.npm '@ai-sdk/openai-compatible'
  Eq 'created config has small_model' $ocJson.small_model 'cgpt/local-title'
  Check 'no .bak for a new opencode.json' (-not (Test-Path -LiteralPath "$Oc.bak"))

  Set-Item -Path $RegKey -Value 'C:\dev\cgpt-bridge\com.cgpt_bridge.host.json'
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Eq 'uninstall keeps a registry key it did not write' (Get-Item -Path $RegKey).GetValue('') 'C:\dev\cgpt-bridge\com.cgpt_bridge.host.json'
  Check 'uninstall leaves no provider' (-not (Has (ReadJson $Oc) 'provider'))
  Check 'uninstall removes the empty install dir' (-not (Test-Path -LiteralPath $App))
  Remove-Item -Path $RegKey -Force

  # -------------------------------------------------------------------------
  Write-Host '# C: user already has small_model and picked cgpt as model'
  # -------------------------------------------------------------------------
  [System.IO.File]::WriteAllText($Oc, '{ "model": "cgpt/chatgpt-web", "small_model": "anthropic/claude-haiku-4-5", "default_agent": "plan" }')
  $r = Install @('-NoStart')
  Eq 'install.ps1 exits 0' $r.Code 0
  $ocJson = ReadJson $Oc
  Eq 'existing small_model kept' $ocJson.small_model 'anthropic/claude-haiku-4-5'
  Eq 'model kept' $ocJson.model 'cgpt/chatgpt-web'
  Eq 'default_agent kept' $ocJson.default_agent 'plan'
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  $ocJson = ReadJson $Oc
  Eq 'uninstall keeps a foreign small_model' $ocJson.small_model 'anthropic/claude-haiku-4-5'
  Check 'uninstall drops a model of the removed provider' (-not (Has $ocJson 'model'))
  Eq 'uninstall keeps default_agent' $ocJson.default_agent 'plan'

  # -------------------------------------------------------------------------
  Write-Host '# D: port 8765 taken by another program'
  # -------------------------------------------------------------------------
  $busy = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 8765)
  $busy.Start()
  try {
    $r = Install
    Check 'install.ps1 fails when the port is taken' ($r.Code -ne 0)
    Check 'the error names the port' ($r.Out -match 'port 8765')
    Check 'a busy port is not reported as started' (-not ($r.Out -match 'gateway answers'))
  } finally {
    $busy.Stop()
  }
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0

  # -------------------------------------------------------------------------
  Write-Host '# E: opencode.json is a symlink (dotfiles)'
  # -------------------------------------------------------------------------
  $dot = Join-Path $Work 'dotfiles'
  New-Item -ItemType Directory -Force -Path $dot | Out-Null
  $target = Join-Path $dot 'opencode.json'
  [System.IO.File]::WriteAllText($target, '{ "theme": "light" }')
  Remove-Item -LiteralPath $Oc, "$Oc.bak" -Force -ErrorAction SilentlyContinue
  $linked = $true
  try { New-Item -ItemType SymbolicLink -Path $Oc -Target $target | Out-Null }
  catch { $linked = $false; Write-Host "skip - cannot create a symlink here ($($_.Exception.Message))" }
  if ($linked) {
    $r = Install @('-NoStart')
    Eq 'install.ps1 with a linked opencode.json exits 0' $r.Code 0
    Eq 'opencode.json is still a symlink' ((Get-Item -LiteralPath $Oc -Force).LinkType) 'SymbolicLink'
    Eq 'the link target got provider cgpt' ((ReadJson $target).provider.cgpt.npm) '@ai-sdk/openai-compatible'
    $r = Uninstall
    Eq 'uninstall.ps1 exits 0' $r.Code 0
    Eq 'opencode.json is still a symlink after uninstall' ((Get-Item -LiteralPath $Oc -Force).LinkType) 'SymbolicLink'
    Check 'uninstall removes cgpt from the link target' (-not (Has (ReadJson $target) 'provider'))
    Remove-Item -LiteralPath $Oc -Force
  }

  # -------------------------------------------------------------------------
  Write-Host "# G: OpenCode agent cgpt next to the user's own agents"
  # -------------------------------------------------------------------------
  if (Test-Path -LiteralPath $OcDir) { Remove-Item -LiteralPath $OcDir -Recurse -Force }
  New-Item -ItemType Directory -Force -Path $AgentDir | Out-Null
  $mine = Join-Path $AgentDir 'mine.md'
  [System.IO.File]::WriteAllText($mine, "---`ndescription: my own agent`nmode: primary`n---`nBe brief.`n", $Utf8NoBom)
  $mineHash = FileHashOf $mine
  $r = Install @('-NoStart')
  Eq 'install.ps1 next to a foreign agent exits 0' $r.Code 0
  Eq 'agent cgpt added next to a foreign agent' (FileHashOf $Agent) (FileHashOf $AgentSrc)
  Eq 'install leaves a foreign agent alone' (FileHashOf $mine) $mineHash

  $agentHash = FileHashOf $Agent
  $r = Install @('-NoStart')
  Eq 'a re-run exits 0' $r.Code 0
  Eq 'a re-run leaves agent cgpt as it was' (FileHashOf $Agent) $agentHash
  Check 'a re-run says the agent is unchanged' ((AgentLine $r.Out) -match 'agent cgpt: unchanged')
  Check 'a re-run leaves no cgpt.md.bak' (-not (Test-Path -LiteralPath "$Agent.bak"))

  # An edited (or older) agent file of ours is brought up to date; the edited
  # copy is kept as cgpt.md.bak.
  $older = ([System.IO.File]::ReadAllText($Agent)).Replace('webfetch: ask', 'webfetch: allow')
  [System.IO.File]::WriteAllText($Agent, $older, $Utf8NoBom)
  $olderHash = FileHashOf $Agent
  $r = Install @('-NoStart')
  Eq 'install.ps1 over an edited agent cgpt exits 0' $r.Code 0
  Eq 'an edited agent cgpt of ours is replaced' (FileHashOf $Agent) (FileHashOf $AgentSrc)
  Eq 'the edited copy is kept as cgpt.md.bak' (FileHashOf "$Agent.bak") $olderHash
  Check 'install says the agent was updated and where the copy is' ((AgentLine $r.Out) -match 'agent cgpt: updated \(previous saved as cgpt\.md\.bak\)')

  # Edit, update, then a newer bundle with a changed template, twice: the edit
  # stays in cgpt.md.bak. The unedited older template is replaced without a
  # .bak, so it never overwrites the saved edit.
  $bundleV2 = Join-Path $Work 'bundle-v2'
  Copy-Item -LiteralPath $Bundle -Destination $bundleV2 -Recurse
  $srcV2 = Join-Path $bundleV2 'opencode-agent-cgpt.md'
  [System.IO.File]::WriteAllText($srcV2, ([System.IO.File]::ReadAllText($srcV2)).Replace("MCP tools and skills`n", "MCP tools and skills (v2)`n"), $Utf8NoBom)
  Check 'the v2 template differs from v1' ((FileHashOf $srcV2) -ne (FileHashOf $AgentSrc))
  $r = Run (Join-Path $bundleV2 'install.ps1') @('-NoStart')
  Eq 'an update to a changed template exits 0' $r.Code 0
  Eq 'the unedited older template is replaced by the new one' (FileHashOf $Agent) (FileHashOf $srcV2)
  Eq 'an unedited older template is reported as updated, without a .bak' (AgentLine $r.Out).TrimEnd() '    agent cgpt: updated'
  Eq 'the saved edit survives a template change' (FileHashOf "$Agent.bak") $olderHash
  $r = Run (Join-Path $bundleV2 'install.ps1') @('-NoStart')
  Eq 'a second v2 install exits 0' $r.Code 0
  Check 'a second v2 install says unchanged' ((AgentLine $r.Out) -match 'agent cgpt: unchanged')
  Eq 'the saved edit survives a second v2 install' (FileHashOf "$Agent.bak") $olderHash
  $r = Install @('-NoStart')
  Eq 'going back to v1 exits 0' $r.Code 0
  Eq 'going back to v1 replaces the unedited v2 file' (FileHashOf $Agent) (FileHashOf $AgentSrc)
  Eq 'the saved edit survives going back' (FileHashOf "$Agent.bak") $olderHash
  Remove-Item -LiteralPath $bundleV2 -Recurse -Force
  Remove-Item -LiteralPath "$Agent.bak" -Force -ErrorAction SilentlyContinue

  # -NoAgent creates nothing and leaves an agent of an earlier run alone.
  [System.IO.File]::WriteAllText($Agent, $older, $Utf8NoBom)
  $olderHash = FileHashOf $Agent
  $r = Install @('-NoStart', '-NoAgent')
  Eq 'install.ps1 -NoAgent exits 0' $r.Code 0
  Eq '-NoAgent leaves an agent cgpt of an earlier run in place' (FileHashOf $Agent) $olderHash
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Check 'uninstall removes agent cgpt' (-not (Test-Path -LiteralPath $Agent))
  Eq 'uninstall keeps an edited agent cgpt as cgpt.md.bak' (FileHashOf "$Agent.bak") $olderHash
  Eq 'uninstall keeps a foreign agent' (FileHashOf $mine) $mineHash
  Check 'uninstall keeps an agents folder that is not empty' (Test-Path -LiteralPath $AgentDir)
  Check 'uninstall says why it kept the copy' ($r.Out -match 'differs from the installed one')
  Remove-Item -LiteralPath "$Agent.bak" -Force -ErrorAction SilentlyContinue
  $r = Install @('-NoStart', '-NoAgent')
  Eq 'install.ps1 -NoAgent exits 0' $r.Code 0
  Check '-NoAgent does not create agent cgpt' (-not (Test-Path -LiteralPath $Agent))
  Check '-NoAgent says so' ((AgentLine $r.Out) -match '-NoAgent')
  Check 'without the agent the installer does not point at Tab' (-not ($r.Out -match 'Tab to switch'))
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0

  # Without the installed template copy (an install from before it existed)
  # the file is kept as .bak to be safe, and the message says so.
  $r = Install @('-NoStart')
  Eq 'install.ps1 exits 0' $r.Code 0
  Remove-Item -LiteralPath (Join-Path $App 'opencode-agent-cgpt.md') -Force
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Check 'without the installed copy uninstall keeps a .bak' (Test-Path -LiteralPath "$Agent.bak")
  Check 'without the installed copy the message does not claim a difference' (-not ($r.Out -match 'differs from the installed one'))
  Check 'without the installed copy the message says why' ($r.Out -match 'to be safe')
  Remove-Item -LiteralPath "$Agent.bak" -Force -ErrorAction SilentlyContinue

  # An agent cgpt the user wrote is not ours: neither install nor uninstall
  # touches it. Both marks are needed, the model alone is not enough.
  foreach ($foreign in "description: my own cgpt`nmodel: openai/gpt-5", "description: my own cgpt`nmodel: cgpt/chatgpt-web") {
    [System.IO.File]::WriteAllText($Agent, "---`n$foreign`n---`nBe brief.`n", $Utf8NoBom)
    $foreignHash = FileHashOf $Agent
    [System.IO.File]::WriteAllText($Oc, '{ "default_agent": "cgpt" }')
    $r = Install @('-NoStart')
    Eq 'install.ps1 next to a foreign agent cgpt exits 0' $r.Code 0
    Eq 'install does not overwrite a foreign agent cgpt' (FileHashOf $Agent) $foreignHash
    Check 'install says the agent was not added' ((AgentLine $r.Out) -match 'not added')
    Check 'without the agent the installer does not point at Tab' (-not ($r.Out -match 'Tab to switch'))
    $r = Uninstall
    Eq 'uninstall.ps1 exits 0' $r.Code 0
    Eq 'uninstall keeps a foreign agent cgpt' (FileHashOf $Agent) $foreignHash
    Eq 'uninstall keeps default_agent cgpt while a foreign agent cgpt stays' (ReadJson $Oc).default_agent 'cgpt'
  }
  Remove-Item -LiteralPath $Agent -Force

  # A cgpt-sub the user wrote is not ours: left alone by install and
  # uninstall, and install warns, since agent cgpt may start it.
  New-Item -ItemType Directory -Force -Path $AgentDir | Out-Null
  [System.IO.File]::WriteAllText($Sub, "---`ndescription: my own helper`nmode: subagent`n---`nBe brief.`n", $Utf8NoBom)
  $foreignSubHash = FileHashOf $Sub
  $r = Install @('-NoStart')
  Eq 'install.ps1 next to a foreign cgpt-sub exits 0' $r.Code 0
  Eq 'install does not overwrite a foreign cgpt-sub' (FileHashOf $Sub) $foreignSubHash
  Check 'install says the subagent was not added' ((SubLine $r.Out) -match 'not added')
  # Windows PowerShell 5.1 wraps long output lines at the console width.
  Check 'install warns that cgpt may start the foreign cgpt-sub' (($r.Out -replace '\r?\n', '') -match 'agent cgpt may start cgpt-sub, which is not ours')
  Eq 'agent cgpt is still added' (FileHashOf $Agent) (FileHashOf $AgentSrc)
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Eq 'uninstall keeps a foreign cgpt-sub' (FileHashOf $Sub) $foreignSubHash
  Check 'uninstall removes agent cgpt next to a foreign cgpt-sub' (-not (Test-Path -LiteralPath $Agent))
  Remove-Item -LiteralPath $Sub -Force
  # An edited cgpt-sub of ours is replaced and kept as cgpt-sub.md.bak.
  $r = Install @('-NoStart')
  Eq 'install.ps1 exits 0' $r.Code 0
  $subEdited = ([System.IO.File]::ReadAllText($Sub)).Replace('  shell: ask', '  shell: allow')
  [System.IO.File]::WriteAllText($Sub, $subEdited, $Utf8NoBom)
  $subEditedHash = FileHashOf $Sub
  $r = Install @('-NoStart')
  Eq 'install.ps1 over an edited cgpt-sub exits 0' $r.Code 0
  Eq 'an edited cgpt-sub of ours is replaced' (FileHashOf $Sub) (FileHashOf $SubSrc)
  Eq 'the edited cgpt-sub is kept as cgpt-sub.md.bak' (FileHashOf "$Sub.bak") $subEditedHash
  Check 'install says where the edited cgpt-sub went' ((SubLine $r.Out) -match 'agent cgpt-sub: updated \(previous saved as cgpt-sub\.md\.bak\)')
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Check 'uninstall removes an unedited cgpt-sub' (-not (Test-Path -LiteralPath $Sub))
  Remove-Item -LiteralPath "$Sub.bak" -Force -ErrorAction SilentlyContinue

  # default_agent cgpt (the user made our agent the default) would point at
  # nothing once the agent is gone: uninstall drops it, like a cgpt/ model.
  [System.IO.File]::WriteAllText($Oc, '{ "default_agent": "cgpt", "theme": "dark" }')
  $r = Install @('-NoStart')
  Eq 'install.ps1 with default_agent cgpt exits 0' $r.Code 0
  Eq 'install leaves default_agent alone' (ReadJson $Oc).default_agent 'cgpt'
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Check 'uninstall removes agent cgpt' (-not (Test-Path -LiteralPath $Agent))
  Check 'uninstall drops default_agent cgpt' (-not (Has (ReadJson $Oc) 'default_agent'))
  Eq 'uninstall keeps the rest of opencode.json' (ReadJson $Oc).theme 'dark'
  # ... but not one that names an agent defined in opencode.json itself.
  [System.IO.File]::WriteAllText($Oc, '{ "default_agent": "cgpt", "agent": { "cgpt": { "model": "openai/gpt-5" } } }')
  $r = Install @('-NoStart')
  Eq 'install.ps1 exits 0' $r.Code 0
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Eq 'uninstall keeps default_agent cgpt of an agent in opencode.json' (ReadJson $Oc).default_agent 'cgpt'
  Remove-Item -LiteralPath $OcDir -Recurse -Force

  # agents\cgpt.md is a folder: not an agent of ours, nothing is said about Tab.
  New-Item -ItemType Directory -Force -Path $Agent | Out-Null
  $r = Install @('-NoStart')
  Eq 'install.ps1 with a folder named cgpt.md exits 0' $r.Code 0
  Check 'install says the agent was not added (folder)' ((AgentLine $r.Out) -match 'not added')
  Eq 'the folder named cgpt.md is left empty' (@(Get-ChildItem -LiteralPath $Agent -Force)).Count 0
  Check 'with a folder in the way the installer does not point at Tab' (-not ($r.Out -match 'Tab to switch'))
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Check 'uninstall leaves the folder named cgpt.md' (Test-Path -LiteralPath $Agent -PathType Container)
  Remove-Item -LiteralPath $OcDir -Recurse -Force

  # The optional agent step cannot fail the install: agents is a file here.
  New-Item -ItemType Directory -Force -Path $OcDir | Out-Null
  [System.IO.File]::WriteAllText($AgentDir, 'not a folder')
  $r = Install @('-NoStart')
  Eq 'install.ps1 exits 0 when the agent cannot be written' $r.Code 0
  Check 'it warns that the agent was not added' ($r.Out -match 'agent cgpt not added')
  Eq 'the install goes on after it (Run value written)' (RunValue $RunName) (ExpectedRunValue)
  Check 'after a failed agent step the installer does not point at Tab' (-not ($r.Out -match 'Tab to switch'))
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
  Remove-Item -LiteralPath $OcDir -Recurse -Force

  # The agents folder is a symlink (dotfiles): the file goes into its target,
  # uninstall removes the file and keeps the link (as rmdir does on macOS).
  New-Item -ItemType Directory -Force -Path $OcDir | Out-Null
  $dotAgents = Join-Path $Work 'dot-agents'
  New-Item -ItemType Directory -Force -Path $dotAgents | Out-Null
  $linked = $true
  try { New-Item -ItemType SymbolicLink -Path $AgentDir -Target $dotAgents | Out-Null }
  catch { $linked = $false; Write-Host "skip - cannot create a symlink here ($($_.Exception.Message))" }
  if ($linked) {
    $r = Install @('-NoStart')
    Eq 'install.ps1 with a linked agents folder exits 0' $r.Code 0
    Eq 'agent cgpt is written into the linked agents folder' (FileHashOf (Join-Path $dotAgents 'cgpt.md')) (FileHashOf $AgentSrc)
    $r = Uninstall
    Eq 'uninstall.ps1 exits 0' $r.Code 0
    Check 'uninstall removes agent cgpt from the linked folder' (-not (Test-Path -LiteralPath (Join-Path $dotAgents 'cgpt.md')))
    Check 'uninstall removes subagent cgpt-sub from the linked folder' (-not (Test-Path -LiteralPath (Join-Path $dotAgents 'cgpt-sub.md')))
    Eq 'uninstall keeps the agents link' ((Get-Item -LiteralPath $AgentDir -Force -ErrorAction SilentlyContinue).LinkType) 'SymbolicLink'
    [System.IO.Directory]::Delete($AgentDir)
  }
  Remove-Item -LiteralPath $OcDir -Recurse -Force

  # The whole opencode folder is a symlink (dotfiles).
  $dotOc = Join-Path $Work 'dot-opencode'
  New-Item -ItemType Directory -Force -Path $dotOc | Out-Null
  $linked = $true
  try { New-Item -ItemType SymbolicLink -Path $OcDir -Target $dotOc | Out-Null }
  catch { $linked = $false; Write-Host "skip - cannot create a symlink here ($($_.Exception.Message))" }
  if ($linked) {
    $r = Install @('-NoStart')
    Eq 'install.ps1 with a linked opencode folder exits 0' $r.Code 0
    Eq 'the opencode folder is still a symlink' ((Get-Item -LiteralPath $OcDir -Force).LinkType) 'SymbolicLink'
    Eq 'agent cgpt is written into the link target' (FileHashOf (Join-Path $dotOc 'agents\cgpt.md')) (FileHashOf $AgentSrc)
    $r = Uninstall
    Eq 'uninstall.ps1 exits 0' $r.Code 0
    Eq 'the opencode folder is still a symlink after uninstall' ((Get-Item -LiteralPath $OcDir -Force).LinkType) 'SymbolicLink'
    Check 'uninstall removes agent cgpt and its folder from the link target' (-not (Test-Path -LiteralPath (Join-Path $dotOc 'agents')))
    # Only the link goes: a non-recursive delete of a directory link does not
    # follow it.
    [System.IO.Directory]::Delete($OcDir)
  }

  # A refused opencode.json means nothing in OpenCode was changed.
  New-Item -ItemType Directory -Force -Path $OcDir | Out-Null
  [System.IO.File]::WriteAllText($Oc, '{ "theme": ')
  $r = Install @('-NoStart')
  Check 'install.ps1 refuses an opencode.json it cannot parse' ($r.Code -ne 0)
  Check 'a refused install adds no agent' (-not (Test-Path -LiteralPath $AgentDir))
  [System.IO.File]::WriteAllText($Oc, '{}')
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0

  # -------------------------------------------------------------------------
  Write-Host '# F: update while Chrome runs the native host (cmd.exe /d /s /c)'
  # -------------------------------------------------------------------------
  $r = Install @('-NoStart')
  Eq 'install.ps1 exits 0' $r.Code 0
  $bin = Join-Path $App 'bin'
  # A host as Chrome runs it: stdin stays open, so it waits for messages and
  # its exe stays locked.
  $h = StartChromeHost (Join-Path $bin 'cgpt-bridge-host.exe')
  try {
    Check 'the host starts through cmd.exe and reports its launch line' `
      (WaitHostLog $h '(?m)^cgpt-bridge-host v\S+ \(build [A-Za-z0-9._-]{1,64}\) started \(pid \d+, socket cgpt-bridge-[^)\s]+\)')
    if ($h.Log) { Write-Host "     host stderr: $($h.Log.Trim())" }
    Check 'the native host runs (its exe is in use)' (-not $h.Proc.HasExited)
    $r = Install @('-NoStart')
    Eq 'install.ps1 with a running host exits 0' $r.Code 0
    if ($r.Code -ne 0) { Write-Host $r.Out }
    Check 'the new host is in place' (Test-Path -LiteralPath (Join-Path $bin 'cgpt-bridge-host.exe'))
    Eq 'the running host was renamed aside' (@(Get-ChildItem -LiteralPath $bin -Filter '*.old')).Count 1
    # Chrome closes the host's stdin when the extension goes away.
    $h.Proc.StandardInput.Close()
    Check 'the host shuts down cleanly when its stdin closes' `
      ((WaitHostLog $h 'cgpt-bridge-host: clean shutdown') -and $h.Proc.WaitForExit(10000))
    if ($h.Proc.HasExited) { Eq 'cmd.exe passes the host exit code through' $h.Proc.ExitCode 0 }
  } finally {
    if (-not $h.Proc.HasExited) {
      & "$env:SystemRoot\System32\taskkill.exe" /F /T /PID $h.Proc.Id 2>&1 | Out-Null
    }
    $h.Proc.WaitForExit(5000) | Out-Null
  }
  $r = Install @('-NoStart')
  Eq 'the next run deletes the renamed host' (@(Get-ChildItem -LiteralPath $bin -Filter '*.old')).Count 0
  $r = Uninstall
  Eq 'uninstall.ps1 exits 0' $r.Code 0
}
finally {
  GatewayProcesses | Stop-Process -Force -ErrorAction SilentlyContinue
  Remove-Item -Path $OtherKey -Force -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $OtherShortcut, $LegacyShortcut -Force -ErrorAction SilentlyContinue
  Remove-ItemProperty -Path $RunKey -Name $OtherRunName, $RunName -ErrorAction SilentlyContinue
  Remove-ItemProperty -Path $ApprovedKey -Name $RunName -ErrorAction SilentlyContinue
  foreach ($k in $Saved.Keys) { Set-Item -Path "env:$k" -Value $Saved[$k] -ErrorAction SilentlyContinue }
  Remove-Item -LiteralPath $WorkRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
if ($script:Fails -gt 0) {
  Write-Host "$($script:Fails) check(s) failed"
  exit 1
}
Write-Host 'all Windows install checks passed'
exit 0
