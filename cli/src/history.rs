//! `cgpt history` / `cgpt replay` / `cgpt last` — offline subcommands that
//! work against `.cgpt-bridge/runs/` without contacting ChatGPT.

use std::time::{Duration, UNIX_EPOCH};

use crate::args::{HistoryArgs, LastArgs, ReplayArgs};
use crate::clipboard;
use crate::plan::{PlanStore, SessionSummary};
use crate::render;
use crate::runner::{escape_control, safe_text};

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
    println!("{}", table_row("SESSION", "STARTED", "STATE", "TASK"));
    for s in sessions.iter().take(limit) {
        let when = format_ts(s.started_at_unix_ms);
        println!(
            "{}",
            table_row(
                &cell(&s.session_id),
                &when,
                &state_of(s),
                &truncate(&cell(s.task.lines().next().unwrap_or("")), 50)
            )
        );
    }
}

/// Width of the STATE column: the longest status, `protocol_violation`.
const STATE_COLS: usize = 18;

/// One line of the table. Task text is truncated by the caller.
fn table_row(session: &str, started: &str, state: &str, task: &str) -> String {
    format!(
        "{:<22} {:<19} {:<w$} {}",
        session,
        started,
        state,
        task,
        w = STATE_COLS
    )
}

/// A table cell read from `.cgpt-bridge/` (a directory name, meta.json,
/// session.json): every control character, line breaks and tabs included,
/// becomes a visible escape, so a crafted file cannot drive the terminal or
/// break the row.
fn cell(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push_str(&escape_control(c.encode_utf8(&mut [0; 4]))),
        }
    }
    out
}

/// The STATE column: the session's own status (`active`, `final`, `idle`,
/// `needs_user_input`, `blocked`, `user_cancelled`, `chat_changed`,
/// `protocol_violation`). A session with no readable status shows `final`
/// when it has a final answer, else `-`. Escaped and cut: it is read from a
/// file.
fn state_of(s: &SessionSummary) -> String {
    match &s.status {
        Some(status) => truncate(&cell(status), STATE_COLS),
        None if s.has_final => "final".into(),
        None => "-".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(status: Option<&str>, has_final: bool) -> SessionSummary {
        SessionSummary {
            session_id: "s19e3c30c179_151c2".into(),
            started_at_unix_ms: 0,
            task: "a task".into(),
            cwd: String::new(),
            has_final,
            status: status.map(str::to_string),
        }
    }

    #[test]
    fn state_shows_the_sessions_status() {
        assert_eq!(
            state_of(&summary(Some("needs_user_input"), false)),
            "needs_user_input"
        );
        assert_eq!(
            state_of(&summary(Some("user_cancelled"), true)),
            "user_cancelled"
        );
        // Before session.json held a status: final.md decides.
        assert_eq!(state_of(&summary(None, true)), "final");
        assert_eq!(state_of(&summary(None, false)), "-");
        // From a file: escaped and cut to the column.
        let odd = state_of(&summary(Some("x\u{1b}[2Jyyyyyyyyyyyyyyyyyyyy"), false));
        assert!(!odd.contains('\u{1b}'), "{odd:?}");
        assert_eq!(odd.chars().count(), STATE_COLS, "{odd:?}");
    }

    #[test]
    fn session_and_task_from_a_crafted_folder_cannot_drive_the_terminal() {
        let raw = "s1\u{1b}]0;pwned\u{7}\u{9b}2J\tx\ry";
        let shown = cell(raw);
        assert!(!shown.chars().any(|c| c.is_control()), "{shown:?}");
        assert_eq!(shown, "s1\\x1b]0;pwned\\x07\\u{9b}2J\\tx\\ry");
        assert_eq!(cell("a\nb"), "a\\nb");
        assert_eq!(cell("plain-id_1"), "plain-id_1");
    }

    #[test]
    fn the_longest_status_keeps_the_task_column_in_place() {
        let head = table_row("SESSION", "STARTED", "STATE", "TASK");
        let row = table_row(
            "s1",
            "2026-11-06 10:00:00",
            "protocol_violation",
            "the task",
        );
        assert_eq!(head.find("TASK"), row.find("the task"), "{head}\n{row}");
    }
}
