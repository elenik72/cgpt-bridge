//! `cgpt history` / `cgpt replay` / `cgpt last` — offline subcommands that
//! work against `.cgpt-bridge/runs/` without contacting ChatGPT.

use std::time::{Duration, UNIX_EPOCH};

use crate::args::{HistoryArgs, LastArgs, ReplayArgs};
use crate::clipboard;
use crate::plan::{PlanStore, SessionSummary};
use crate::render;
use crate::runner::safe_text;

pub fn run_history(args: HistoryArgs) -> u8 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot get current dir: {}", e);
            return 1;
        }
    };
    let sessions = match PlanStore::list_sessions(&root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cgpt: cannot read .cgpt-bridge/runs/: {}", e);
            return 1;
        }
    };
    if sessions.is_empty() {
        eprintln!("cgpt: no agent sessions found in {}", root.display());
        return 0;
    }
    print_table(&sessions, args.limit);
    0
}

pub fn run_replay(args: ReplayArgs) -> u8 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot get current dir: {}", e);
            return 1;
        }
    };
    render_session(&root, &args.session_id, args.no_pretty, args.copy)
}

pub fn run_last(args: LastArgs) -> u8 {
    let root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot get current dir: {}", e);
            return 1;
        }
    };
    let id = match PlanStore::latest_session_id(&root) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("cgpt: no agent sessions found in {}", root.display());
            return 1;
        }
        Err(e) => {
            eprintln!("cgpt: cannot read sessions: {}", e);
            return 1;
        }
    };
    render_session(&root, &id, args.no_pretty, args.copy)
}

fn render_session(
    project_root: &std::path::Path,
    session_id: &str,
    no_pretty: bool,
    copy: bool,
) -> u8 {
    let text = match PlanStore::read_final_markdown(project_root, session_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "cgpt: no final.md for session `{}`: {}.\n\
                 The session may have ended without `status: final`.",
                session_id, e
            );
            return 1;
        }
    };
    // The stored text came from the model: escape control characters so a
    // replay cannot drive the terminal.
    let shown = safe_text(&text);
    if no_pretty {
        print!("{}", shown);
        if !shown.ends_with('\n') {
            println!();
        }
    } else {
        render::print_markdown(&shown);
    }
    if copy {
        if let Err(e) = clipboard::write(&text) {
            eprintln!("cgpt: --copy: clipboard write failed: {}", e);
        }
    }
    0
}

fn print_table(sessions: &[SessionSummary], limit: usize) {
    // Column widths tuned for an 80-wide terminal. Truncate task text.
    println!(
        "{:<22} {:<19} {:<6} TASK",
        "SESSION", "STARTED", "STATE"
    );
    for s in sessions.iter().take(limit) {
        let when = format_ts(s.started_at_unix_ms);
        let state = if s.has_final { "final" } else { "open" };
        let task = truncate(&s.task, 60);
        println!("{:<22} {:<19} {:<6} {}", s.session_id, when, state, task);
    }
}

pub(crate) fn format_ts(ms: u64) -> String {
    if ms == 0 {
        return "(unknown)".into();
    }
    // Local-ish ISO format without pulling chrono. SystemTime's Debug is
    // `SystemTime { tv_sec: X, tv_nsec: Y }` on Linux which is ugly, so
    // build a small UTC string manually.
    let secs = ms / 1000;
    let t = UNIX_EPOCH + Duration::from_secs(secs);
    match t.duration_since(UNIX_EPOCH) {
        Ok(_) => {
            let (y, mo, d, h, mi, se) = cgpt_bridge_core::time::ymdhms_utc(secs);
            format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, se)
        }
        Err(_) => "(invalid)".into(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one_line: String = s.lines().next().unwrap_or("").to_string();
    if one_line.chars().count() <= n {
        one_line
    } else {
        let mut taken: String = one_line.chars().take(n.saturating_sub(1)).collect();
        taken.push('…');
        taken
    }
}
