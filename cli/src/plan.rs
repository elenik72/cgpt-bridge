//! Local plan storage for `cgpt agent`.
//!
//! Layout (per `docs/requirements.md` §6 and `docs/architecture.md` §2.6):
//!
//! ```text
//! .cgpt-bridge/
//!   plan.jsonl                       # append-only event log, source of truth
//!   plan.md                          # regenerated from plan.jsonl
//!   session.json                     # current session metadata
//!   runs/<session-id>/
//!     transcript.jsonl               # message-level transcript (redacted)
//!     command-<id>.json              # per-command (redacted) record
//!   logs/                            # reserved; CLI/host text logs (future)
//! ```
//!
//! Crash-safe rules:
//!   - `plan.jsonl` is append-only with newline-terminated JSON per line.
//!     A SIGINT mid-append leaves the file parseable line-by-line.
//!   - `plan.md` and `session.json` are written via temp+rename so a crash
//!     never leaves a half-written file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cgpt_bridge_protocol::agent::{
    AgentResponseV1, CommandRequest, CommandResultV1, PlanEvent, PlanUpdate, Status, TaskStatus,
};
use serde::{Deserialize, Serialize};

pub const DEFAULT_DIR_NAME: &str = ".cgpt-bridge";

/// One entry in `plan.jsonl`. Wrapping the protocol events in our own enum
/// gives us room to record meta-events (session_started, command_run, ...)
/// that have no corresponding ChatGPT-side PlanEvent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanEntry {
    SessionStarted {
        session_id: String,
        cwd: String,
        task: String,
        at_unix_ms: u64,
    },
    Summary {
        text: String,
        at_unix_ms: u64,
    },
    Goal {
        text: String,
        at_unix_ms: u64,
    },
    Task {
        id: String,
        status: String,
        text: String,
        at_unix_ms: u64,
    },
    Finding {
        text: String,
        at_unix_ms: u64,
    },
    Decision {
        text: String,
        at_unix_ms: u64,
    },
    Note {
        text: String,
        at_unix_ms: u64,
    },
    Warning {
        text: String,
        at_unix_ms: u64,
    },
    CommandProposed {
        command_id: String,
        cwd: String,
        command: String,
        risk_declared: String,
        at_unix_ms: u64,
    },
    CommandResult {
        command_id: String,
        exit_code: Option<i32>,
        duration_ms: u64,
        timed_out: bool,
        redactions_applied: Vec<String>,
        at_unix_ms: u64,
    },
    UserCancelled {
        reason: String,
        at_unix_ms: u64,
    },
    Final {
        text: String,
        at_unix_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: String,
    pub started_at_unix_ms: u64,
    pub cwd: String,
    pub task_summary: String,
    pub last_turn_at_unix_ms: u64,
    pub status: String,
}

/// Static per-session metadata. Written once when the session opens and
/// then read by `cgpt history` / `cgpt replay` / `cgpt last` to enumerate
/// past runs without scanning the project-wide `plan.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub started_at_unix_ms: u64,
    pub cwd: String,
    pub task: String,
}

/// Public view of a past session for listing UIs.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub started_at_unix_ms: u64,
    pub task: String,
    pub cwd: String,
    /// True if a `final.md` file is present in the run dir.
    pub has_final: bool,
}

pub struct PlanStore {
    root: PathBuf,
    plan_jsonl: PathBuf,
    plan_md: PathBuf,
    session_json: PathBuf,
    runs_dir: PathBuf,
    logs_dir: PathBuf,
    session_id: String,
}

impl PlanStore {
    /// Open (or create) the `.cgpt-bridge/` directory at `project_root` and
    /// initialise structure for a new session.
    pub fn open(project_root: &Path, session_id: String, task: &str) -> io::Result<Self> {
        let root = project_root.join(DEFAULT_DIR_NAME);
        fs::create_dir_all(&root)?;
        let plan_jsonl = root.join("plan.jsonl");
        let plan_md = root.join("plan.md");
        let session_json = root.join("session.json");
        let runs_dir = root.join("runs").join(&session_id);
        let logs_dir = root.join("logs");
        fs::create_dir_all(&runs_dir)?;
        fs::create_dir_all(&logs_dir)?;

        let store = PlanStore {
            root,
            plan_jsonl,
            plan_md,
            session_json,
            runs_dir,
            logs_dir,
            session_id: session_id.clone(),
        };

        let now = now_ms();
        store.append_entry(&PlanEntry::SessionStarted {
            session_id: session_id.clone(),
            cwd: project_root.display().to_string(),
            task: task.to_string(),
            at_unix_ms: now,
        })?;
        store.write_session_metadata(&SessionMetadata {
            session_id: session_id.clone(),
            started_at_unix_ms: now,
            cwd: project_root.display().to_string(),
            task_summary: task.to_string(),
            last_turn_at_unix_ms: now,
            status: "active".into(),
        })?;
        let meta = SessionMeta {
            session_id,
            started_at_unix_ms: now,
            cwd: project_root.display().to_string(),
            task: task.to_string(),
        };
        write_atomic(
            &store.runs_dir.join("meta.json"),
            serde_json::to_vec_pretty(&meta).unwrap().as_slice(),
        )?;
        store.regenerate_plan_md()?;
        Ok(store)
    }

    /// Re-open an existing session for `--continue`. Does not append a
    /// SessionStarted entry — the original run already did. Used to keep
    /// `plan.jsonl` and `runs/<id>/` growing for the same session id.
    pub fn open_existing(project_root: &Path, session_id: String) -> io::Result<Self> {
        let root = project_root.join(DEFAULT_DIR_NAME);
        let runs_dir = root.join("runs").join(&session_id);
        if !runs_dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("session not found: {}", session_id),
            ));
        }
        let store = PlanStore {
            plan_jsonl: root.join("plan.jsonl"),
            plan_md: root.join("plan.md"),
            session_json: root.join("session.json"),
            logs_dir: root.join("logs"),
            runs_dir,
            root,
            session_id,
        };
        Ok(store)
    }

    /// Enumerate past sessions, newest first. Sessions without a
    /// `runs/<id>/meta.json` (older format) are still returned with empty
    /// task/cwd so the user can still see them in `cgpt history`.
    pub fn list_sessions(project_root: &Path) -> io::Result<Vec<SessionSummary>> {
        let runs = project_root.join(DEFAULT_DIR_NAME).join("runs");
        if !runs.is_dir() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&runs)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let session_id = entry.file_name().to_string_lossy().to_string();
            let dir = entry.path();
            let meta: Option<SessionMeta> = fs::read(dir.join("meta.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            let has_final = dir.join("final.md").is_file();
            let (started, task, cwd) = match meta {
                Some(m) => (m.started_at_unix_ms, m.task, m.cwd),
                None => {
                    // Fallback: use directory mtime so list_sessions still
                    // sorts something useful for pre-meta runs.
                    let started = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    (started, String::new(), String::new())
                }
            };
            out.push(SessionSummary {
                session_id,
                started_at_unix_ms: started,
                task,
                cwd,
                has_final,
            });
        }
        out.sort_by(|a, b| b.started_at_unix_ms.cmp(&a.started_at_unix_ms));
        Ok(out)
    }

    /// Read the archived final markdown for a given session, if any.
    pub fn read_final_markdown(project_root: &Path, session_id: &str) -> io::Result<String> {
        let p = project_root
            .join(DEFAULT_DIR_NAME)
            .join("runs")
            .join(session_id)
            .join("final.md");
        fs::read_to_string(&p)
    }

    /// Convenience: id of the most recent session, if any.
    pub fn latest_session_id(project_root: &Path) -> io::Result<Option<String>> {
        Ok(Self::list_sessions(project_root)?
            .into_iter()
            .next()
            .map(|s| s.session_id))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runs_dir(&self) -> &Path {
        &self.runs_dir
    }

    pub fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }

    /// Append one entry. `plan.jsonl` is opened in append mode each call so
    /// concurrent processes do not have to coordinate file-position locks.
    pub fn append_entry(&self, entry: &PlanEntry) -> io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.plan_jsonl)?;
        let line = serde_json::to_string(entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("serialize: {}", e)))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_data()?;
        Ok(())
    }

    /// Apply a PlanUpdate from an AgentResponseV1: emit one Summary entry +
    /// one entry per event. Unknown event variants are skipped silently.
    pub fn apply_plan_update(&self, update: &PlanUpdate) -> io::Result<()> {
        let now = now_ms();
        if !update.summary.trim().is_empty() {
            self.append_entry(&PlanEntry::Summary {
                text: update.summary.clone(),
                at_unix_ms: now,
            })?;
        }
        for ev in &update.events {
            if let Some(entry) = plan_event_to_entry(ev, now) {
                self.append_entry(&entry)?;
            }
        }
        self.regenerate_plan_md()?;
        Ok(())
    }

    pub fn record_command_proposed(&self, cmd: &CommandRequest) -> io::Result<()> {
        let entry = PlanEntry::CommandProposed {
            command_id: cmd.id.clone(),
            cwd: cmd.cwd.clone(),
            command: cmd.command.clone(),
            risk_declared: format!("{:?}", cmd.risk).to_lowercase(),
            at_unix_ms: now_ms(),
        };
        self.append_entry(&entry)
    }

    pub fn record_command_result(&self, result: &CommandResultV1) -> io::Result<()> {
        // Persist the per-command file (already-redacted contents).
        let path = self
            .runs_dir
            .join(format!("command-{}.json", result.command_id));
        write_atomic(&path, serde_json::to_vec_pretty(result).unwrap().as_slice())?;
        // Append a compact summary to plan.jsonl.
        let entry = PlanEntry::CommandResult {
            command_id: result.command_id.clone(),
            exit_code: result.exit_code,
            duration_ms: result.duration_ms,
            timed_out: result.timed_out,
            redactions_applied: result.redactions_applied.clone(),
            at_unix_ms: now_ms(),
        };
        self.append_entry(&entry)
    }

    pub fn record_user_cancelled(&self, reason: &str) -> io::Result<()> {
        self.append_entry(&PlanEntry::UserCancelled {
            reason: reason.to_string(),
            at_unix_ms: now_ms(),
        })
    }

    /// On `status: final`, archive the final assistant message text.
    /// Writes both the per-event `Final` entry to `plan.jsonl` *and* a
    /// `runs/<id>/final.md` copy so `cgpt replay`/`cgpt last` can render
    /// the message later without re-scanning the project-wide event log.
    pub fn record_final(&self, response: &AgentResponseV1) -> io::Result<()> {
        if matches!(response.status, Status::Final) {
            self.append_entry(&PlanEntry::Final {
                text: response.user_message.clone(),
                at_unix_ms: now_ms(),
            })?;
            write_atomic(
                &self.runs_dir.join("final.md"),
                response.user_message.as_bytes(),
            )?;
        }
        Ok(())
    }

    /// Append a single line to the per-session transcript. Caller decides
    /// what shape the line takes (raw assistant text, parsed envelope, etc).
    pub fn transcript_append(&self, line_json: &serde_json::Value) -> io::Result<()> {
        let path = self.runs_dir.join("transcript.jsonl");
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        f.write_all(line_json.to_string().as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_data()
    }

    pub fn update_session_status(&self, status: &str) -> io::Result<()> {
        // Load existing, mutate, write back atomically.
        let mut meta = self.read_session_metadata().unwrap_or(SessionMetadata {
            session_id: self.session_id.clone(),
            started_at_unix_ms: now_ms(),
            cwd: String::new(),
            task_summary: String::new(),
            last_turn_at_unix_ms: now_ms(),
            status: status.into(),
        });
        meta.status = status.into();
        meta.last_turn_at_unix_ms = now_ms();
        self.write_session_metadata(&meta)
    }

    fn read_session_metadata(&self) -> Option<SessionMetadata> {
        let bytes = fs::read(&self.session_json).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_session_metadata(&self, meta: &SessionMetadata) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(meta).unwrap();
        write_atomic(&self.session_json, &bytes)
    }

    /// Rebuild plan.md from plan.jsonl. Cheap because plans stay small;
    /// regenerated on every PlanUpdate so the markdown is always in sync.
    pub fn regenerate_plan_md(&self) -> io::Result<()> {
        let raw = match fs::read_to_string(&self.plan_jsonl) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let entries: Vec<PlanEntry> = raw
            .lines()
            .filter_map(|l| {
                let trimmed = l.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    serde_json::from_str::<PlanEntry>(trimmed).ok()
                }
            })
            .collect();
        let md = render_plan_md(&entries, &self.session_id);
        write_atomic(&self.plan_md, md.as_bytes())
    }
}

fn plan_event_to_entry(ev: &PlanEvent, at_unix_ms: u64) -> Option<PlanEntry> {
    match ev {
        PlanEvent::Goal { text } => Some(PlanEntry::Goal {
            text: text.clone(),
            at_unix_ms,
        }),
        PlanEvent::Task { id, status, text } => Some(PlanEntry::Task {
            id: id.clone(),
            status: task_status_to_string(*status).into(),
            text: text.clone(),
            at_unix_ms,
        }),
        PlanEvent::Finding { text } => Some(PlanEntry::Finding {
            text: text.clone(),
            at_unix_ms,
        }),
        PlanEvent::Decision { text } => Some(PlanEntry::Decision {
            text: text.clone(),
            at_unix_ms,
        }),
        PlanEvent::Note { text } => Some(PlanEntry::Note {
            text: text.clone(),
            at_unix_ms,
        }),
        PlanEvent::Warning { text } => Some(PlanEntry::Warning {
            text: text.clone(),
            at_unix_ms,
        }),
        PlanEvent::Unknown => None,
    }
}

fn task_status_to_string(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Todo => "todo",
        TaskStatus::Doing => "doing",
        TaskStatus::Done => "done",
        TaskStatus::Blocked => "blocked",
    }
}

fn render_plan_md(entries: &[PlanEntry], session_id: &str) -> String {
    let mut goals = Vec::new();
    let mut tasks = Vec::new();
    let mut findings = Vec::new();
    let mut decisions = Vec::new();
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    let mut commands = Vec::new();
    let mut last_summary = String::new();
    let mut started_at = 0u64;
    let mut task_text_summary = String::new();
    let mut final_text: Option<String> = None;

    for e in entries {
        match e {
            PlanEntry::SessionStarted {
                task, at_unix_ms, ..
            } => {
                started_at = *at_unix_ms;
                task_text_summary = task.clone();
            }
            PlanEntry::Summary { text, .. } => last_summary = text.clone(),
            PlanEntry::Goal { text, .. } => goals.push(text.clone()),
            PlanEntry::Task {
                id, status, text, ..
            } => tasks.push((id.clone(), status.clone(), text.clone())),
            PlanEntry::Finding { text, .. } => findings.push(text.clone()),
            PlanEntry::Decision { text, .. } => decisions.push(text.clone()),
            PlanEntry::Note { text, .. } => notes.push(text.clone()),
            PlanEntry::Warning { text, .. } => warnings.push(text.clone()),
            PlanEntry::CommandProposed {
                command_id,
                command,
                cwd,
                risk_declared,
                ..
            } => commands.push((
                command_id.clone(),
                cwd.clone(),
                command.clone(),
                risk_declared.clone(),
                None::<i32>,
                false,
            )),
            PlanEntry::CommandResult {
                command_id,
                exit_code,
                timed_out,
                ..
            } => {
                if let Some(slot) = commands.iter_mut().find(|c| &c.0 == command_id) {
                    slot.4 = *exit_code;
                    slot.5 = *timed_out;
                }
            }
            PlanEntry::UserCancelled { .. } => {}
            PlanEntry::Final { text, .. } => final_text = Some(text.clone()),
        }
    }

    let mut out = String::new();
    out.push_str(
        "<!-- generated from plan.jsonl by `cgpt agent` — hand edits are NOT preserved -->\n",
    );
    out.push_str("# Agent plan\n\n");
    out.push_str(&format!("- Session: `{}`\n", session_id));
    out.push_str(&format!("- Started: {}\n", format_unix_ms(started_at)));
    if !task_text_summary.is_empty() {
        out.push_str(&format!("- Task: {}\n", task_text_summary));
    }
    if !last_summary.is_empty() {
        out.push_str(&format!("- Status summary: {}\n", last_summary));
    }
    out.push('\n');

    if !goals.is_empty() {
        out.push_str("## Goals\n\n");
        for g in &goals {
            out.push_str(&format!("- {}\n", g));
        }
        out.push('\n');
    }
    if !tasks.is_empty() {
        out.push_str("## Tasks\n\n");
        for (id, status, text) in &tasks {
            let mark = match status.as_str() {
                "done" => "[x]",
                "doing" => "[~]",
                "blocked" => "[!]",
                _ => "[ ]",
            };
            out.push_str(&format!("- {} `{}` {}\n", mark, id, text));
        }
        out.push('\n');
    }
    if !findings.is_empty() {
        out.push_str("## Findings\n\n");
        for x in &findings {
            out.push_str(&format!("- {}\n", x));
        }
        out.push('\n');
    }
    if !decisions.is_empty() {
        out.push_str("## Decisions\n\n");
        for x in &decisions {
            out.push_str(&format!("- {}\n", x));
        }
        out.push('\n');
    }
    if !warnings.is_empty() {
        out.push_str("## Warnings\n\n");
        for x in &warnings {
            out.push_str(&format!("- {}\n", x));
        }
        out.push('\n');
    }
    if !notes.is_empty() {
        out.push_str("## Notes\n\n");
        for x in &notes {
            out.push_str(&format!("- {}\n", x));
        }
        out.push('\n');
    }
    if !commands.is_empty() {
        out.push_str("## Commands\n\n");
        for (id, cwd, command, risk, exit, to) in &commands {
            let status = match (exit, to) {
                (_, true) => "timeout".to_string(),
                (Some(code), false) => format!("exit {}", code),
                (None, false) => "(no result yet)".into(),
            };
            out.push_str(&format!(
                "- `{}` [{}] in `{}` — {} — `{}`\n",
                id, risk, cwd, status, command
            ));
        }
        out.push('\n');
    }
    if let Some(t) = final_text {
        out.push_str("## Final\n\n");
        out.push_str(&t);
        out.push_str("\n");
    }

    out
}

fn format_unix_ms(ms: u64) -> String {
    // Minimal ISO-ish format without pulling chrono.
    if ms == 0 {
        return "(unknown)".into();
    }
    let secs = (ms / 1000) as i64;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    // Best-effort: format via SystemTime so the OS does the calendar math.
    let st = std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos);
    format!("{:?}", st)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = BufWriter::new(File::create(&tmp)?);
        f.write_all(bytes)?;
        f.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn new_session_id() -> String {
    let ms = now_ms();
    let pid = std::process::id();
    format!("s{:x}_{:x}", ms, pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cgpt_bridge_protocol::agent::{CommandKind, PlanUpdate, Risk, SendOutput, Status};

    fn tmp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let base = PathBuf::from("/tmp").join(format!(
            "cgb-plan-test-{}-{}-{}",
            std::process::id(),
            nanos,
            n,
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn open_creates_layout() {
        let root = tmp_root();
        let store = PlanStore::open(&root, new_session_id(), "diagnose failing tests").unwrap();
        assert!(root.join(".cgpt-bridge").is_dir());
        assert!(store.runs_dir().is_dir());
        assert!(store.logs_dir().is_dir());
        assert!(root.join(".cgpt-bridge/plan.jsonl").is_file());
        assert!(root.join(".cgpt-bridge/session.json").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_plan_update_writes_events_and_regens_md() {
        let root = tmp_root();
        let store = PlanStore::open(&root, "s1".into(), "task").unwrap();
        let update = PlanUpdate {
            summary: "scoping".into(),
            events: vec![
                PlanEvent::Goal {
                    text: "find bug".into(),
                },
                PlanEvent::Task {
                    id: "T1".into(),
                    status: TaskStatus::Doing,
                    text: "repro".into(),
                },
                PlanEvent::Unknown,
            ],
        };
        store.apply_plan_update(&update).unwrap();
        let md = fs::read_to_string(root.join(".cgpt-bridge/plan.md")).unwrap();
        assert!(md.contains("find bug"));
        assert!(md.contains("`T1`"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn command_result_writes_per_command_file_and_summary() {
        let root = tmp_root();
        let store = PlanStore::open(&root, "s2".into(), "task").unwrap();
        let cmd = CommandRequest {
            id: "cmd_001".into(),
            kind: CommandKind::Shell,
            description: "x".into(),
            cwd: ".".into(),
            command: "ls".into(),
            expected_effect: "list".into(),
            risk: Risk::ReadOnly,
            timeout_ms: 1000,
            send_output: SendOutput::Summary,
        };
        store.record_command_proposed(&cmd).unwrap();
        let res = CommandResultV1 {
            version: 1,
            command_id: "cmd_001".into(),
            cwd: ".".into(),
            command: "ls".into(),
            exit_code: Some(0),
            duration_ms: 5,
            timed_out: false,
            stdout: "a\n".into(),
            stderr: "".into(),
            output_truncated: false,
            redactions_applied: vec![],
        };
        store.record_command_result(&res).unwrap();
        let per_cmd = root.join(".cgpt-bridge/runs/s2/command-cmd_001.json");
        assert!(per_cmd.is_file());
        let jsonl = fs::read_to_string(root.join(".cgpt-bridge/plan.jsonl")).unwrap();
        assert!(jsonl.contains("\"command_proposed\""));
        assert!(jsonl.contains("\"command_result\""));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn final_response_is_recorded() {
        let root = tmp_root();
        let store = PlanStore::open(&root, "s3".into(), "task").unwrap();
        let resp = AgentResponseV1 {
            version: 1,
            status: Status::Final,
            user_message: "all done".into(),
            plan_update: PlanUpdate {
                summary: "".into(),
                events: vec![],
            },
            command: None,
        };
        store.record_final(&resp).unwrap();
        let jsonl = fs::read_to_string(root.join(".cgpt-bridge/plan.jsonl")).unwrap();
        assert!(jsonl.contains("\"kind\":\"final\""));
        assert!(jsonl.contains("\"all done\""));
        let _ = fs::remove_dir_all(&root);
    }
}
