//! Default Unix domain socket path used by both the host (server) and the CLI
//! (client). Centralised here so the two sides cannot drift apart.
//!
//! Resolution rules (in order):
//!   1. `$XDG_RUNTIME_DIR/cgpt-bridge.sock` if XDG_RUNTIME_DIR is set and
//!      non-empty (typical Linux user-session path).
//!   2. `$TMPDIR/cgpt-bridge.<uid>.sock` otherwise (macOS path).
//!
//! macOS keeps `sun_path` at 104 bytes, and the standard `$TMPDIR`
//! (`/var/folders/...`) is already ~49 chars. Our short filename keeps us
//! well clear of that cap.

use std::path::PathBuf;

pub fn default_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("cgpt-bridge.sock");
        }
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(tmp).join(format!("cgpt-bridge.{}.sock", current_uid()))
}

fn current_uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both branches of `default_socket_path` are exercised in a single test
    // because they both mutate process-wide env vars. Splitting them would
    // race under the default parallel test runner.
    #[test]
    fn resolves_both_branches() {
        let prev_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let prev_tmp = std::env::var("TMPDIR").ok();

        // Branch 1: XDG_RUNTIME_DIR set → that path wins.
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/xdg-test");
        let p = default_socket_path();
        assert_eq!(p, PathBuf::from("/tmp/xdg-test/cgpt-bridge.sock"));

        // Branch 2: XDG_RUNTIME_DIR unset → fall back to TMPDIR + uid.
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::set_var("TMPDIR", "/var/folders/test");
        let p = default_socket_path();
        let s = p.to_string_lossy();
        assert!(s.starts_with("/var/folders/test/cgpt-bridge."));
        assert!(s.ends_with(".sock"));

        // Restore.
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        match prev_tmp {
            Some(v) => std::env::set_var("TMPDIR", v),
            None => std::env::remove_var("TMPDIR"),
        }
    }
}
