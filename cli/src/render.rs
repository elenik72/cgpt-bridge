//! Pretty-print markdown text via `glow` when available, falling back to
//! the raw text otherwise. Used to render the final assistant message of a
//! `cgpt agent` session.
//!
//! Fallback policy (per the user-confirmed plan):
//!   - If `glow` is not on PATH, print the raw markdown to stdout and emit a
//!     one-line install hint on stderr. The agent run still succeeds; only
//!     the rendering is downgraded.
//!   - If stdout is not a TTY (the output is being piped / redirected), skip
//!     `glow` entirely and print raw markdown so downstream consumers don't
//!     receive ANSI escapes.

use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

/// Print `text` to stdout. Tries `glow -s auto -` when stdout is a TTY; on
/// any failure (binary missing, non-zero exit, write error) falls back to
/// printing the raw text. A single trailing newline is guaranteed so callers
/// don't have to think about it.
pub fn print_markdown(text: &str) {
    if !std::io::stdout().is_terminal() {
        print_raw(text);
        return;
    }
    match render_with_glow(text) {
        Ok(rendered) => {
            print!("{}", rendered);
            if !rendered.ends_with('\n') {
                println!();
            }
        }
        Err(RenderErr::NotFound) => {
            eprintln!(
                "cgpt: `glow` not found on PATH — falling back to raw markdown.\n\
                 Install glow for prettier output: `brew install glow` (macOS) or see https://github.com/charmbracelet/glow."
            );
            print_raw(text);
        }
        Err(RenderErr::Other(msg)) => {
            eprintln!("cgpt: glow rendering failed: {} — falling back to raw.", msg);
            print_raw(text);
        }
    }
}

fn print_raw(text: &str) {
    print!("{}", text);
    if !text.ends_with('\n') {
        println!();
    }
}

enum RenderErr {
    NotFound,
    Other(String),
}

fn render_with_glow(text: &str) -> Result<String, RenderErr> {
    let mut child = match Command::new("glow")
        .args(["-s", "auto", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RenderErr::NotFound),
        Err(e) => return Err(RenderErr::Other(e.to_string())),
    };

    if let Some(stdin) = child.stdin.as_mut() {
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            return Err(RenderErr::Other(format!("write to glow stdin: {}", e)));
        }
    }
    // Dropping stdin closes it so glow finishes.
    drop(child.stdin.take());

    let out = child
        .wait_with_output()
        .map_err(|e| RenderErr::Other(format!("wait glow: {}", e)))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(RenderErr::Other(if stderr.is_empty() {
            format!("glow exited {}", out.status)
        } else {
            stderr
        }));
    }
    String::from_utf8(out.stdout).map_err(|e| RenderErr::Other(format!("glow stdout: {}", e)))
}
