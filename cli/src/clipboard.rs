//! Read the OS clipboard as a UTF-8 string.
//!
//! Shells out instead of pulling in a clipboard crate so the install
//! footprint stays minimal and Linux doesn't need to link X11/Wayland libs
//! at build time. The user is expected to have the platform's standard
//! clipboard tool already installed (preinstalled on macOS; one apt/pacman
//! install on Linux).

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Try the platform's standard clipboard reader. Returns the captured
/// stdout as a String on success.
pub fn read() -> io::Result<String> {
    for (bin, args) in candidates() {
        match try_capture(bin, args) {
            Ok(Some(text)) => return Ok(text),
            Ok(None) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        platform_install_hint(),
    ))
}

/// Write `text` to the OS clipboard. Symmetric to `read()`: shells out to
/// `pbcopy` (macOS) or `wl-copy`/`xclip`/`xsel` (Linux).
pub fn write(text: &str) -> io::Result<()> {
    for (bin, args) in write_candidates() {
        match try_write(bin, args, text) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        platform_install_hint(),
    ))
}

fn try_write(bin: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    drop(child.stdin.take());
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!("{} exited {}", bin, status)));
    }
    Ok(())
}

fn write_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    #[cfg(target_os = "macos")]
    {
        &[("pbcopy", &[])]
    }
    #[cfg(target_os = "linux")]
    {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

fn candidates() -> &'static [(&'static str, &'static [&'static str])] {
    #[cfg(target_os = "macos")]
    {
        &[("pbpaste", &[])]
    }
    #[cfg(target_os = "linux")]
    {
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

fn platform_install_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "no clipboard reader found (expected `pbpaste`, which ships with macOS)"
    }
    #[cfg(target_os = "linux")]
    {
        "no clipboard reader found. Install one: `wl-paste` (wl-clipboard) for Wayland, or `xclip`/`xsel` for X11"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "clipboard reading is not supported on this OS in v0.1"
    }
}

fn try_capture(bin: &str, args: &[&str]) -> io::Result<Option<String>> {
    let out = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    match String::from_utf8(out.stdout) {
        Ok(s) => Ok(Some(s)),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} returned non-UTF-8 bytes: {}", bin, e),
        )),
    }
}
