//! Open `$EDITOR` on a tmpfile so the user can compose the prompt/task in
//! a real editor instead of fighting shell quoting. Mirrors the pattern
//! `git commit` uses.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Open the user's editor on a tmpfile, optionally pre-populated, and
/// return the resulting contents with the editor's trailing newline left
/// intact (the caller will normalize as needed).
pub fn capture(initial: &str) -> io::Result<String> {
    let path = tmp_path();
    fs::write(&path, initial)?;

    let editor = pick_editor();
    let status = Command::new(&editor.bin)
        .args(&editor.args)
        .arg(&path)
        .status()?;
    if !status.success() {
        let _ = fs::remove_file(&path);
        return Err(io::Error::other(format!(
            "editor `{}` exited {}",
            editor.bin, status
        )));
    }

    let out = fs::read_to_string(&path)?;
    let _ = fs::remove_file(&path);
    Ok(out)
}

struct EditorCmd {
    bin: String,
    args: Vec<String>,
}

/// Resolve which editor to launch. Honors `$VISUAL` then `$EDITOR`. Falls
/// back to `vi`, then `nano`. The env var may include arguments
/// (e.g. `"code --wait"`); we split on whitespace conservatively.
fn pick_editor() -> EditorCmd {
    for var in &["VISUAL", "EDITOR"] {
        if let Ok(val) = env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                let mut parts = trimmed.split_whitespace();
                let bin = parts.next().unwrap().to_string();
                let args = parts.map(|s| s.to_string()).collect();
                return EditorCmd { bin, args };
            }
        }
    }
    for bin in &["vi", "nano"] {
        if which(bin).is_some() {
            return EditorCmd {
                bin: bin.to_string(),
                args: vec![],
            };
        }
    }
    // Last-ditch — let the OS report a clear "not found" later.
    EditorCmd {
        bin: "vi".to_string(),
        args: vec![],
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn tmp_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut p = env::temp_dir();
    p.push(format!("cgpt-prompt-{}-{}.md", pid, nanos));
    p
}
