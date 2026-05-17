# cgpt-bridge — install on a new machine

This tarball contains:

- `bin/cgpt` — the CLI.
- `bin/cgpt-bridge-host` — the Chrome Native Messaging host.
- `extension/` — the unpacked Chrome extension (load this folder at chrome://extensions).
- `install/macos/install-host.sh` and `install/linux/install-host.sh` — Native Messaging manifest installers.

## Steps

1. **Load the extension.**
   - Open `chrome://extensions`.
   - Enable *Developer mode* (top right).
   - Click *Load unpacked*, choose the `extension/` folder from this bundle.
   - The extension id is fixed via the `key` field in the manifest, so it is
     the same on every machine. You do not need to copy it — the installer
     below already knows it.

2. **Install the Native Messaging manifest.**
   - macOS:  `./install/macos/install-host.sh`
   - Linux:  `./install/linux/install-host.sh`
     Use `--chromium` if you loaded the extension in Chromium instead of Chrome.

   The script writes a manifest pinning the host to the pinned extension id
   and pointing it at `bin/cgpt-bridge-host` in this folder. Do not move the
   folder after running the installer — the manifest stores an absolute path.

3. **Reload the extension.** Click the refresh icon on the extension card.

4. **Verify.** Open the extension's service-worker DevTools (link on the
   extension card), and in its Console run:
   ```
   pingNative()
   ```
   You should see `{ type: "pong", ... }`.

5. **Drop `bin/cgpt` somewhere on your `PATH`** (or call it via its absolute
   path):
   ```
   cp bin/cgpt /usr/local/bin/cgpt          # macOS
   cp bin/cgpt ~/.local/bin/cgpt            # Linux (typical user-local)
   ```

6. **Smoke test.** Open https://chatgpt.com in any window, click anywhere
   inside that tab once (required for the background-tab keep-alive). Then:
   ```
   cgpt ask "hello"
   cgpt agent --yolo "list files"
   ```

## Hidden-tab caveat

The bridge can drive ChatGPT while the tab is in the background. To prevent
Chrome from throttling timers on the hidden tab, the extension plays a
silent audio stream when sending a prompt. Chrome's autoplay policy requires
the page to have received at least one user gesture (a click, keypress,
etc.) since it loaded. If you reopen the ChatGPT tab fresh and immediately
hide it without clicking, the audio context cannot resume and you'll see
slow responses; a single click anywhere in the tab unblocks it for the
session.

## Uninstall

- Delete the Native Messaging manifest:
  - macOS:  `rm "$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.cgpt_bridge.host.json"`
  - Linux:  `rm "$HOME/.config/google-chrome/NativeMessagingHosts/com.cgpt_bridge.host.json"` (or `chromium`)
- Remove the extension at `chrome://extensions`.
- Delete this folder.
