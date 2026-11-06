<#
.SYNOPSIS
  cgpt-bridge - Windows release bundler.

.DESCRIPTION
  Builds the host, the gateway and the CLI in release mode, builds the
  extension and packs dist\cgpt-bridge-<version>-<build>-windows-x64.zip
  ($env:DIST_DIR stages somewhere else than dist\). <build> is the commit's
  12-character short SHA ($env:CGPT_BUILD overrides it, `unknown` without
  .git, `-dirty` appended when the tree has uncommitted changes, the same
  rule as scripts/build-id.sh); the binaries and the extension carry the
  same id:

    bin\cgpt-gateway.exe, bin\cgpt-bridge-host.exe
    extension\
    install.ps1, uninstall.ps1, start-gateway.ps1, opencode-provider.json,
    opencode-agent-cgpt.md, opencode-agent-cgpt-sub.md (shared with the macOS
    bundle)
    INSTALL.md, README.md, VERSION
  and writes SHA256SUMS (checksum of the zip) next to it.

  The CLI is built (it must compile on Windows) but not shipped: its agent
  runs commands through a POSIX shell, which is not ported to Windows.

  Usage (from the repository root):
    pwsh ./scripts/build-release.ps1
#>
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$match = Select-String -Path 'Cargo.toml' -Pattern '^version = "([^"]+)"' | Select-Object -First 1
if (-not $match) { throw 'could not read the workspace version from Cargo.toml' }
$version = $match.Matches[0].Groups[1].Value

# The build id (rule of scripts/build-id.sh), read by option_env! in the
# protocol crate and by the extension build.
$build = $env:CGPT_BUILD
if (-not $build) {
  $build = 'unknown'
  # Only this tree's own .git: a tarball unpacked inside another repository
  # must not take that repository's commit.
  if ((Test-Path -LiteralPath (Join-Path $Root '.git')) -and (Get-Command git -ErrorAction SilentlyContinue)) {
    try {
      # In its own scope with 'Continue': Windows PowerShell 5.1 turns git's
      # stderr into an error record, which 'Stop' would make terminating.
      $sha = & { $ErrorActionPreference = 'Continue'; git -C $Root rev-parse --short=12 HEAD 2>$null }
      if ($LASTEXITCODE -eq 0 -and $sha) {
        $build = "$sha".Trim()
        # Uncommitted changes (tracked or untracked, not ignored) mark the
        # build; a failing `git status` counts as dirty. The SHA part is cut
        # so the suffix stays within the 64 characters checked below.
        $changes = & { $ErrorActionPreference = 'Continue'; git -C $Root status --porcelain 2>$null }
        if ($LASTEXITCODE -ne 0 -or $changes) {
          if ($build.Length -gt 58) { $build = $build.Substring(0, 58) }
          $build = "$build-dirty"
        }
      }
    } catch { }
  }
}
# \z, not $: in .NET `$` also matches before a final newline.
if ($build -cnotmatch '^[A-Za-z0-9._-]{1,64}\z') {
  throw "build id '$build' must be 1-64 characters of A-Z a-z 0-9 . _ -"
}
$env:CGPT_BUILD = $build

Write-Host "==> Building Rust release binaries (build $build)"
cargo build --release --locked -p cgpt-bridge-host -p cgpt-gateway -p cgpt-bridge-cli
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }

Write-Host '==> Building Chrome extension'
Push-Location 'extension'
try {
  npm ci --silent --no-audit --no-fund
  if ($LASTEXITCODE -ne 0) { throw "npm ci failed (exit $LASTEXITCODE)" }
  npm run build
  if ($LASTEXITCODE -ne 0) { throw "npm run build failed (exit $LASTEXITCODE)" }
} finally {
  Pop-Location
}

$dist = if ($env:DIST_DIR) { $env:DIST_DIR } else { Join-Path $Root 'dist' }
$name = "cgpt-bridge-$version-$build-windows-x64"
$stage = Join-Path $dist $name
$zip = "$stage.zip"

Write-Host "==> Staging bundle at $stage"
foreach ($old in $stage, $zip) {
  if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old -Recurse -Force }
}
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'bin') | Out-Null
foreach ($exe in 'cgpt-gateway.exe', 'cgpt-bridge-host.exe') {
  Copy-Item -LiteralPath (Join-Path 'target\release' $exe) -Destination (Join-Path $stage "bin\$exe")
}
Copy-Item -LiteralPath 'extension\dist' -Destination (Join-Path $stage 'extension') -Recurse
foreach ($f in 'install.ps1', 'uninstall.ps1', 'start-gateway.ps1', 'opencode-provider.json') {
  Copy-Item -LiteralPath (Join-Path 'install\windows' $f) -Destination $stage
}
foreach ($f in 'opencode-agent-cgpt.md', 'opencode-agent-cgpt-sub.md') {
  Copy-Item -LiteralPath (Join-Path 'install' $f) -Destination $stage
}
Copy-Item -LiteralPath 'README.md' -Destination $stage
Set-Content -LiteralPath (Join-Path $stage 'VERSION') -Value $version
Set-Content -LiteralPath (Join-Path $stage 'INSTALL.md') -Value @'
# cgpt-bridge - install on Windows

1. In this folder run (no admin rights needed):

       powershell -ExecutionPolicy Bypass -File .\install.ps1

   It copies everything to %LOCALAPPDATA%\cgpt-bridge\, registers the native
   host with Chrome (HKCU), creates the gateway config with its token, adds
   provider "cgpt" to %USERPROFILE%\.config\opencode\opencode.json (the
   previous file is kept as opencode.json.bak), adds the OpenCode agents cgpt
   and cgpt-sub (%USERPROFILE%\.config\opencode\agents\; -NoAgent skips
   them), and
   starts the gateway now and at every login (HKCU Run value cgpt-gateway),
   then checks that it answers. Chrome may stay open during an update.
2. Chrome -> chrome://extensions -> Developer mode -> Load unpacked ->
   %LOCALAPPDATA%\cgpt-bridge\extension. After an update press the reload
   button on the extension card: Chrome keeps running the old native host
   until then. Then close the old cgpt-bridge window.
3. Open https://chatgpt.com and log in. Keep chat history on and do not use
   a temporary chat. Do not run Chrome as administrator.
4. In OpenCode press Tab to switch to the agent cgpt (or run
   opencode --agent cgpt). It uses the model cgpt/chatgpt-web and asks
   before every command, file edit, web access, MCP tool and skill. It is
   not the default
   agent: pick it with Tab each time. The model cgpt/chatgpt-web chosen in
   another agent works too, but nothing asks there.
5. On the first request the extension opens its own ChatGPT window without
   focus; OpenCode never uses your own ChatGPT tabs. Leave that window open,
   do not type in it, and click once inside its page (again after it was
   fully reloaded) so it keeps streaming in the background. Each OpenCode
   session becomes a chat in your ChatGPT history.

Uninstall (in PowerShell):

       powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\cgpt-bridge\uninstall.ps1"

then remove the extension at chrome://extensions.

See README.md (section "Install") for OpenCode permissions, skills and
troubleshooting.
'@

Write-Host "==> Creating $zip"
Compress-Archive -Path $stage -DestinationPath $zip

# SHA256SUMS next to the zip, in `sha256sum` format: lowercase hex, two
# spaces, file name, LF line end, no BOM (so `sha256sum -c` reads it too).
$sums = Join-Path $dist 'SHA256SUMS'
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -LiteralPath $sums -Value ("$hash  $name.zip`n") -NoNewline -Encoding ascii

Write-Host ''
Write-Host "Built bundle: $zip"
Write-Host "Checksum:     $sums"
Write-Host "Staged tree:  $stage"
