//! Agent-mode protocol: types and parser for `cgpt-agent-response-v1` and
//! `cgpt-command-result-v1`, plus the prompt contract the CLI prepends on
//! the first turn of `cgpt agent`.
//!
//! The wire spec lives in `docs/protocol.md`. This module is the canonical
//! Rust mirror of that spec. The CLI parses ChatGPT's assistant text with
//! `parse_agent_response`, never with ad-hoc regex elsewhere.

use serde::{Deserialize, Serialize};

use crate::fence::fenced_blocks;

// ---------------------------------------------------------------------------
// AgentResponseV1 (ChatGPT -> CLI)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentResponseV1 {
    /// Protocol version. Must be `1`.
    pub version: u32,
    /// One of: `continue`, `final`, `blocked`, `needs_user_input`.
    pub status: Status,
    /// Human-readable text shown to the user (may be empty).
    pub user_message: String,
    /// Structured plan delta.
    pub plan_update: PlanUpdate,
    /// One command proposal, or `null` to skip a turn.
    pub command: Option<CommandRequest>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Continue,
    Final,
    Blocked,
    NeedsUserInput,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlanUpdate {
    pub summary: String,
    pub events: Vec<PlanEvent>,
}

/// PlanEvent uses serde's internally-tagged enum so unknown variants surface
/// as `Unknown` rather than failing the whole turn (lenient nested policy).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanEvent {
    Goal {
        text: String,
    },
    Task {
        id: String,
        status: TaskStatus,
        text: String,
    },
    Finding {
        text: String,
    },
    Decision {
        text: String,
    },
    Note {
        text: String,
    },
    Warning {
        text: String,
    },
    /// Forward-compatible escape hatch. Unknown event types deserialize here
    /// (via #[serde(other)]) and are logged + skipped by the caller.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    Doing,
    Done,
    Blocked,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandRequest {
    pub id: String,
    pub kind: CommandKind,
    pub description: String,
    pub cwd: String,
    pub command: String,
    pub expected_effect: String,
    /// Advisory only. The local CLI runs its own classifier.
    pub risk: Risk,
    pub timeout_ms: u64,
    pub send_output: SendOutput,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Shell,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    ReadOnly,
    WriteLocal,
    Network,
    Destructive,
    SecretRisk,
    Privileged,
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SendOutput {
    Summary,
    Truncated,
    Full,
}

// ---------------------------------------------------------------------------
// CommandResultV1 (CLI -> ChatGPT)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandResultV1 {
    pub version: u32,
    pub command_id: String,
    pub cwd: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub redactions_applied: Vec<String>,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub const AGENT_FENCE_INFO: &str = "cgpt-agent-response-v1";
pub const COMMAND_RESULT_FENCE_INFO: &str = "cgpt-command-result-v1";

/// Hard cap on `timeout_ms`. Values above this are clamped + a warning is
/// emitted to the user; values <= 0 are rejected at the schema layer.
pub const DEFAULT_TIMEOUT_CAP_MS: u64 = 600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Zero fenced `cgpt-agent-response-v1` blocks in the assistant text.
    NoBlock,
    /// More than one such block (rejected per protocol §5).
    DuplicateBlocks { count: usize },
    /// Body did not parse as JSON.
    BadJson { detail: String },
    /// `version` is not the integer `1` (or wrong type).
    UnknownVersion { got: serde_json::Value },
    /// Schema validation failed (missing required field, wrong type, ...).
    SchemaInvalid { detail: String },
    /// Command-level rule failed (empty command, kind != shell, etc.).
    InvalidCommand { detail: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NoBlock => {
                write!(f, "missing required `cgpt-agent-response-v1` fenced block")
            }
            ParseError::DuplicateBlocks { count } => write!(
                f,
                "expected exactly one `cgpt-agent-response-v1` block, got {}",
                count
            ),
            ParseError::BadJson { detail } => write!(f, "invalid JSON in block: {}", detail),
            ParseError::UnknownVersion { got } => {
                write!(f, "unknown protocol version: {}", got)
            }
            ParseError::SchemaInvalid { detail } => {
                write!(f, "schema invalid: {}", detail)
            }
            ParseError::InvalidCommand { detail } => {
                write!(f, "invalid command: {}", detail)
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Outcome of parsing a turn. The CLI may downgrade `timeout_ms` if it
/// exceeds the configured cap; `timeout_clamped` records that for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAgentResponse {
    pub response: AgentResponseV1,
    pub unknown_event_count: usize,
    pub timeout_clamped: Option<TimeoutClamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutClamp {
    pub requested_ms: u64,
    pub clamped_to_ms: u64,
}

/// Parse the full assistant message text. Extracts the one fenced
/// `cgpt-agent-response-v1` block and validates it. Applies the
/// `DEFAULT_TIMEOUT_CAP_MS` clamp if needed.
pub fn parse_agent_response(text: &str) -> Result<ParsedAgentResponse, ParseError> {
    parse_agent_response_with_cap(text, DEFAULT_TIMEOUT_CAP_MS)
}

pub fn parse_agent_response_with_cap(
    text: &str,
    timeout_cap_ms: u64,
) -> Result<ParsedAgentResponse, ParseError> {
    let bodies = extract_fenced_bodies(text, AGENT_FENCE_INFO);
    match bodies.len() {
        0 => return Err(ParseError::NoBlock),
        1 => {}
        n => return Err(ParseError::DuplicateBlocks { count: n }),
    }
    let body = &bodies[0];

    // Inspect raw JSON first so we can produce specific errors for version
    // and unknown-top-level-field cases before serde's generic message.
    let raw: serde_json::Value = serde_json::from_str(body).map_err(|e| ParseError::BadJson {
        detail: e.to_string(),
    })?;
    validate_version(&raw)?;

    let response: AgentResponseV1 = serde_json::from_value(raw).map_err(|e| {
        // serde with deny_unknown_fields surfaces unknown top-level fields
        // here. We pass the detail through verbatim — the user prompt to
        // ChatGPT in the repair flow will show exactly what was wrong.
        ParseError::SchemaInvalid {
            detail: e.to_string(),
        }
    })?;

    let unknown_event_count = response
        .plan_update
        .events
        .iter()
        .filter(|e| matches!(e, PlanEvent::Unknown))
        .count();

    let mut timeout_clamped = None;
    let response = if let Some(cmd) = &response.command {
        validate_command(cmd)?;
        if cmd.timeout_ms > timeout_cap_ms {
            let mut clamped = response.clone();
            if let Some(c) = clamped.command.as_mut() {
                c.timeout_ms = timeout_cap_ms;
            }
            timeout_clamped = Some(TimeoutClamp {
                requested_ms: cmd.timeout_ms,
                clamped_to_ms: timeout_cap_ms,
            });
            clamped
        } else {
            response
        }
    } else {
        response
    };

    Ok(ParsedAgentResponse {
        response,
        unknown_event_count,
        timeout_clamped,
    })
}

fn validate_version(raw: &serde_json::Value) -> Result<(), ParseError> {
    match raw.get("version") {
        Some(v) if v == &serde_json::json!(1) => Ok(()),
        Some(v) => Err(ParseError::UnknownVersion { got: v.clone() }),
        None => Err(ParseError::SchemaInvalid {
            detail: "missing required field `version`".into(),
        }),
    }
}

/// Longest accepted command `id`.
const MAX_COMMAND_ID_LEN: usize = 64;

/// `id` comes from the model and ends up in a file name (`command-<id>.json`),
/// so it must be a plain token: `[A-Za-z0-9_.-]{1,64}` with no `..`.
fn is_safe_command_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_COMMAND_ID_LEN
        && !id.contains("..")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Unicode bidirectional controls (LRM/RLM/ALM, embeddings, overrides,
/// isolates) and zero-width characters (ZWSP, ZWNJ, ZWJ, word joiner, BOM).
fn is_invisible_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{061c}'
            | '\u{200b}'..='\u{200d}'
            | '\u{2060}'
            | '\u{feff}'
    )
}

fn validate_command(cmd: &CommandRequest) -> Result<(), ParseError> {
    if !is_safe_command_id(&cmd.id) {
        return Err(ParseError::InvalidCommand {
            detail: format!(
                "id must match [A-Za-z0-9_.-]{{1,{}}} and not contain \"..\"",
                MAX_COMMAND_ID_LEN
            ),
        });
    }
    if cmd.command.trim().is_empty() {
        return Err(ParseError::InvalidCommand {
            detail: "command string is empty".into(),
        });
    }
    // Control characters (CR and ESC can repaint the terminal, the rest make
    // the confirmation panel ambiguous) have no place in a shell command.
    // Newline and tab are fine.
    if cmd
        .command
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(ParseError::InvalidCommand {
            detail: "command must not contain control characters other than newline and tab".into(),
        });
    }
    // Invisible formatting characters reorder or hide what the panel shows
    // (a reversed-looking command, a word that differs from what runs).
    // Right-to-left letters and emoji stay legal in prose fields; only the
    // command is held to this.
    if cmd.command.chars().any(is_invisible_format_char) {
        return Err(ParseError::InvalidCommand {
            detail: "command must not contain bidirectional or zero-width characters".into(),
        });
    }
    if cmd.timeout_ms == 0 {
        return Err(ParseError::InvalidCommand {
            detail: "timeout_ms must be > 0".into(),
        });
    }
    // `kind` is currently constrained at the type level to Shell. If we add
    // new kinds we will enforce them here.
    let _ = cmd.kind;
    Ok(())
}

/// Extract the body of every top-level, closed fenced code block whose
/// fence is three or more backticks indented by at most three spaces, and
/// whose info string is exactly `info` (case-sensitive). Blocks come from
/// `crate::fence`: an example quoted inside another code block is content of
/// that block, and a fence indented by four or more spaces or a tab is an
/// indented code sample in CommonMark — neither is a protocol block. An
/// unclosed block is dropped — the assistant text was malformed and the
/// caller will see NoBlock or DuplicateBlocks depending on what else is
/// there.
fn extract_fenced_bodies(text: &str, info: &str) -> Vec<String> {
    fenced_blocks(text)
        .into_iter()
        .filter(|b| {
            b.closed
                && b.fence.ch == '`'
                && b.indent.len() <= 3
                && b.indent.bytes().all(|c| c == b' ')
                && b.info == info
        })
        .map(|b| b.body)
        .collect()
}

// ---------------------------------------------------------------------------
// CommandResultV1 wire helper
// ---------------------------------------------------------------------------

/// Render a `CommandResultV1` as the next user-message body to send back into
/// the ChatGPT composer. The format mirrors §3 / §4.3 of `docs/protocol.md`:
/// a single fenced block with info string `cgpt-command-result-v1`, optional
/// short comment line above for the user reading the chat.
pub fn format_command_result(result: &CommandResultV1) -> String {
    let mut buf = String::new();
    buf.push_str("```");
    buf.push_str(COMMAND_RESULT_FENCE_INFO);
    buf.push('\n');
    let pretty = serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".into());
    buf.push_str(&pretty);
    buf.push('\n');
    buf.push_str("```\n");
    buf
}

// ---------------------------------------------------------------------------
// Repair prompt + Agent prompt contract
// ---------------------------------------------------------------------------

pub const REPAIR_PROMPT: &str = "Your previous response did not match the required protocol. Return exactly one valid `cgpt-agent-response-v1` fenced JSON block. Do not include any other fenced protocol blocks. Do not include prose explanations of the JSON. Do not change the schema. If you cannot continue, return a block with `status: \"blocked\"` and `command: null`.";

pub const AGENT_PROMPT_CONTRACT: &str = include_str!("agent_prompt_contract.md");

#[cfg(test)]
mod tests {
    use super::*;

    fn block(body: &str) -> String {
        format!("```{}\n{}\n```\n", AGENT_FENCE_INFO, body)
    }

    fn minimal_final() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "status": "final",
            "user_message": "done",
            "plan_update": { "summary": "", "events": [] },
            "command": null,
        })
    }

    #[test]
    fn parses_minimal_final_block() {
        let text = block(&minimal_final().to_string());
        let parsed = parse_agent_response(&text).unwrap();
        assert_eq!(parsed.response.status, Status::Final);
        assert!(parsed.response.command.is_none());
        assert_eq!(parsed.unknown_event_count, 0);
        assert!(parsed.timeout_clamped.is_none());
    }

    #[test]
    fn no_block_error() {
        let err = parse_agent_response("just prose, no fences").unwrap_err();
        assert!(matches!(err, ParseError::NoBlock), "got {:?}", err);
    }

    #[test]
    fn two_blocks_rejected() {
        let body = minimal_final().to_string();
        let text = format!("{}{}", block(&body), block(&body));
        let err = parse_agent_response(&text).unwrap_err();
        assert!(matches!(err, ParseError::DuplicateBlocks { count: 2 }));
    }

    #[test]
    fn unknown_version_rejected() {
        let mut v = minimal_final();
        v["version"] = serde_json::json!(2);
        let err = parse_agent_response(&block(&v.to_string())).unwrap_err();
        match err {
            ParseError::UnknownVersion { got } => assert_eq!(got, serde_json::json!(2)),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn string_version_rejected() {
        let mut v = minimal_final();
        v["version"] = serde_json::json!("1");
        let err = parse_agent_response(&block(&v.to_string())).unwrap_err();
        assert!(matches!(err, ParseError::UnknownVersion { .. }));
    }

    #[test]
    fn missing_required_field_rejected() {
        let mut v = minimal_final();
        v.as_object_mut().unwrap().remove("user_message");
        let err = parse_agent_response(&block(&v.to_string())).unwrap_err();
        assert!(matches!(err, ParseError::SchemaInvalid { .. }));
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let mut v = minimal_final();
        v["extra"] = serde_json::json!(1);
        let err = parse_agent_response(&block(&v.to_string())).unwrap_err();
        assert!(matches!(err, ParseError::SchemaInvalid { .. }));
    }

    #[test]
    fn unknown_event_type_is_lenient() {
        let mut v = minimal_final();
        v["plan_update"]["events"] = serde_json::json!([
            {"type":"goal","text":"do it"},
            {"type":"weird_thing","extra":1}
        ]);
        let parsed = parse_agent_response(&block(&v.to_string())).unwrap();
        assert_eq!(parsed.unknown_event_count, 1);
        assert_eq!(parsed.response.plan_update.events.len(), 2);
    }

    #[test]
    fn full_example_with_command_parses() {
        let v = serde_json::json!({
            "version": 1,
            "status": "continue",
            "user_message": "running tests",
            "plan_update": {
                "summary": "Starting",
                "events": [
                    {"type":"goal","text":"diagnose"},
                    {"type":"task","id":"T1","status":"doing","text":"run cargo test"},
                ],
            },
            "command": {
                "id":"cmd_001",
                "kind":"shell",
                "description":"run tests",
                "cwd":".",
                "command":"cargo test 2>&1 | tee out.log",
                "expected_effect":"runs",
                "risk":"write_local",
                "timeout_ms": 120000,
                "send_output":"truncated"
            },
        });
        let parsed = parse_agent_response(&block(&v.to_string())).unwrap();
        let cmd = parsed.response.command.unwrap();
        assert_eq!(cmd.id, "cmd_001");
        assert_eq!(cmd.risk, Risk::WriteLocal);
        assert_eq!(cmd.timeout_ms, 120000);
    }

    #[test]
    fn empty_command_rejected() {
        let mut v = minimal_final();
        v["status"] = serde_json::json!("continue");
        v["command"] = serde_json::json!({
            "id":"c","kind":"shell","description":"x","cwd":".",
            "command":"   ",
            "expected_effect":"x","risk":"read_only","timeout_ms":1000,"send_output":"summary",
        });
        let err = parse_agent_response(&block(&v.to_string())).unwrap_err();
        assert!(matches!(err, ParseError::InvalidCommand { .. }));
    }

    fn command_with_id(id: &str) -> String {
        let mut v = minimal_final();
        v["status"] = serde_json::json!("continue");
        v["command"] = serde_json::json!({
            "id": id, "kind":"shell","description":"x","cwd":".",
            "command":"ls",
            "expected_effect":"x","risk":"read_only","timeout_ms":1000,"send_output":"summary",
        });
        block(&v.to_string())
    }

    #[test]
    fn unsafe_command_ids_rejected() {
        let long = "a".repeat(65);
        for id in [
            "../x",
            "a/b",
            "a\\b",
            "a..b",
            "..",
            "",
            "has space",
            "tab\there",
            "nl\nx",
            "nul\u{0}x",
            "ctl\u{7f}",
            "ünï",
            long.as_str(),
        ] {
            let err = parse_agent_response(&command_with_id(id)).unwrap_err();
            assert!(
                matches!(err, ParseError::InvalidCommand { .. }),
                "id {:?} should be rejected, got {:?}",
                id,
                err
            );
        }
    }

    #[test]
    fn safe_command_ids_accepted() {
        let max = "a".repeat(64);
        for id in [
            "cmd_001",
            "c",
            "run-1.2",
            "A_b-C.9",
            ".hidden",
            max.as_str(),
        ] {
            let parsed = parse_agent_response(&command_with_id(id)).unwrap();
            assert_eq!(parsed.response.command.unwrap().id, id);
        }
    }

    fn command_with_text(command: &str) -> String {
        let mut v = minimal_final();
        v["status"] = serde_json::json!("continue");
        v["command"] = serde_json::json!({
            "id": "c", "kind":"shell","description":"x","cwd":".",
            "command": command,
            "expected_effect":"x","risk":"read_only","timeout_ms":1000,"send_output":"summary",
        });
        block(&v.to_string())
    }

    #[test]
    fn command_with_cr_or_esc_rejected() {
        for cmd in [
            "ls\rrm -rf x",
            "echo hi\r",
            "\rls",
            "echo \u{1b}[2K\u{1b}[1Aok",
            "\u{1b}]0;title\u{7}ls",
            "ls\r\nrm x",
        ] {
            let err = parse_agent_response(&command_with_text(cmd)).unwrap_err();
            assert!(
                matches!(err, ParseError::InvalidCommand { .. }),
                "command {:?} should be rejected, got {:?}",
                cmd,
                err
            );
        }
    }

    #[test]
    fn command_with_any_other_control_char_rejected() {
        for cmd in [
            "ls\u{0}x",
            "echo \u{7}",
            "rm a\u{8}b",
            "ls\u{7f}",
            "echo \u{9b}31m",
            "echo \u{85}x",
            "echo \u{b}x",
            "echo \u{c}x",
        ] {
            let err = parse_agent_response(&command_with_text(cmd)).unwrap_err();
            assert!(
                matches!(err, ParseError::InvalidCommand { .. }),
                "command {:?} should be rejected, got {:?}",
                cmd,
                err
            );
        }
    }

    #[test]
    fn command_with_bidi_or_zero_width_chars_rejected() {
        for c in [
            '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}', '\u{200e}', '\u{200f}', '\u{061c}', '\u{200b}', '\u{200c}',
            '\u{200d}', '\u{2060}', '\u{feff}',
        ] {
            let cmd = format!("echo {}ok", c);
            let err = parse_agent_response(&command_with_text(&cmd)).unwrap_err();
            assert!(
                matches!(err, ParseError::InvalidCommand { .. }),
                "command {:?} should be rejected, got {:?}",
                cmd,
                err
            );
        }
    }

    #[test]
    fn command_with_rtl_letters_still_accepted() {
        let cmd = "echo \u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{627}\u{644}\u{633}\u{644}\u{627}\u{645}";
        let parsed = parse_agent_response(&command_with_text(cmd)).unwrap();
        assert_eq!(parsed.response.command.unwrap().command, cmd);
    }

    #[test]
    fn prose_fields_may_carry_rtl_text_and_zwj_sequences() {
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let desc = format!("\u{5e9}\u{5dc}\u{5d5}\u{5dd} {} \u{200f}end", family);
        let mut v = minimal_final();
        v["status"] = serde_json::json!("continue");
        v["command"] = serde_json::json!({
            "id": "c", "kind":"shell","description": desc,"cwd":".",
            "command": "ls",
            "expected_effect": desc,"risk":"read_only","timeout_ms":1000,"send_output":"summary",
        });
        let parsed = parse_agent_response(&block(&v.to_string())).unwrap();
        assert_eq!(parsed.response.command.unwrap().description, desc);
    }

    #[test]
    fn multiline_command_with_lf_and_tab_still_accepted() {
        let cmd = "set -e\n\tls\n\techo ok";
        let parsed = parse_agent_response(&command_with_text(cmd)).unwrap();
        assert_eq!(parsed.response.command.unwrap().command, cmd);
    }

    #[test]
    fn zero_timeout_rejected() {
        let mut v = minimal_final();
        v["status"] = serde_json::json!("continue");
        v["command"] = serde_json::json!({
            "id":"c","kind":"shell","description":"x","cwd":".",
            "command":"ls",
            "expected_effect":"x","risk":"read_only","timeout_ms":0,"send_output":"summary",
        });
        let err = parse_agent_response(&block(&v.to_string())).unwrap_err();
        assert!(matches!(err, ParseError::InvalidCommand { .. }));
    }

    #[test]
    fn oversized_timeout_is_clamped_not_rejected() {
        let mut v = minimal_final();
        v["status"] = serde_json::json!("continue");
        v["command"] = serde_json::json!({
            "id":"c","kind":"shell","description":"x","cwd":".",
            "command":"ls",
            "expected_effect":"x","risk":"read_only",
            "timeout_ms": DEFAULT_TIMEOUT_CAP_MS + 1,
            "send_output":"summary",
        });
        let parsed = parse_agent_response(&block(&v.to_string())).unwrap();
        let clamp = parsed.timeout_clamped.expect("expected clamp record");
        assert_eq!(clamp.requested_ms, DEFAULT_TIMEOUT_CAP_MS + 1);
        assert_eq!(clamp.clamped_to_ms, DEFAULT_TIMEOUT_CAP_MS);
        assert_eq!(
            parsed.response.command.unwrap().timeout_ms,
            DEFAULT_TIMEOUT_CAP_MS
        );
    }

    #[test]
    fn format_command_result_round_trips() {
        let r = CommandResultV1 {
            version: 1,
            command_id: "cmd_001".into(),
            cwd: ".".into(),
            command: "ls".into(),
            exit_code: Some(0),
            duration_ms: 12,
            timed_out: false,
            stdout: "a\nb\n".into(),
            stderr: "".into(),
            output_truncated: false,
            redactions_applied: vec![],
        };
        let s = format_command_result(&r);
        assert!(s.starts_with("```cgpt-command-result-v1\n"));
        assert!(s.contains("\"version\": 1"));
        assert!(s.contains("\"command_id\": \"cmd_001\""));
        assert!(s.trim_end().ends_with("```"));
    }

    #[test]
    fn fence_extraction_handles_surrounding_prose() {
        let text = format!(
            "hello\nsome prose\n```{}\n{}\n```\ntrailing prose\n",
            AGENT_FENCE_INFO,
            minimal_final()
        );
        let parsed = parse_agent_response(&text).unwrap();
        assert_eq!(parsed.response.status, Status::Final);
    }

    #[test]
    fn fence_extraction_ignores_other_info_strings() {
        let text = format!(
            "```json\n{{}}\n```\n```{}\n{}\n```\n",
            AGENT_FENCE_INFO,
            minimal_final()
        );
        let parsed = parse_agent_response(&text).unwrap();
        assert_eq!(parsed.response.status, Status::Final);
    }

    #[test]
    fn corpus_counts_only_top_level_backtick_blocks_with_the_exact_label() {
        for case in crate::fence::corpus::FENCE_CORPUS {
            // Closed, backticks, exact label, and at most three spaces before
            // the opening fence (more, or a tab, makes an indented code
            // sample in CommonMark).
            let ours = case
                .blocks
                .iter()
                .filter(|e| {
                    e.closed
                        && e.fence == '`'
                        && e.info == AGENT_FENCE_INFO
                        && e.indent.len() <= 3
                        && e.indent.bytes().all(|b| b == b' ')
                })
                .count();
            let got = parse_agent_response(case.input);
            match ours {
                0 => assert_eq!(got, Err(ParseError::NoBlock), "{}", case.name),
                1 => assert!(got.is_ok(), "{}: {:?}", case.name, got),
                n => assert_eq!(
                    got,
                    Err(ParseError::DuplicateBlocks { count: n }),
                    "{}",
                    case.name
                ),
            }
        }
    }

    fn ls_command() -> String {
        let mut v = minimal_final();
        v["status"] = serde_json::json!("continue");
        v["command"] = serde_json::json!({
            "id":"cmd_001","kind":"shell","description":"x","cwd":".",
            "command":"ls -la",
            "expected_effect":"x","risk":"read_only","timeout_ms":1000,"send_output":"summary",
        });
        v.to_string()
    }

    #[test]
    fn example_nested_in_a_code_block_is_not_a_command() {
        // A quoted example is content of the outer block (CommonMark), never
        // a proposal: four-backtick, tilde and longer-run outer fences.
        let inner = block(&ls_command());
        for (open, close) in [
            ("````markdown", "````"),
            ("~~~md", "~~~"),
            ("`````", "``````"),
        ] {
            let text = format!("Format:\n{}\n{}{}\nThat is all.\n", open, inner, close);
            assert_eq!(
                parse_agent_response(&text),
                Err(ParseError::NoBlock),
                "outer fence {:?}",
                open
            );
        }
        // Quoted example next to a real block: the real one wins, alone.
        let text = format!(
            "````markdown\n{}````\n{}",
            inner,
            block(&minimal_final().to_string())
        );
        let parsed = parse_agent_response(&text).unwrap();
        assert!(parsed.response.command.is_none());
    }

    #[test]
    fn lookalike_labels_are_not_the_protocol_block() {
        let body = ls_command();
        for info in [
            "cgpt-agent-response-v1x",
            "CGPT-AGENT-RESPONSE-V1",
            "Cgpt-agent-response-v1",
            "cgpt-agent-response-v1 json",
            "json cgpt-agent-response-v1",
        ] {
            let text = format!("```{}\n{}\n```\n", info, body);
            assert_eq!(
                parse_agent_response(&text),
                Err(ParseError::NoBlock),
                "info {:?}",
                info
            );
        }
        // Tilde fences are display-only: the protocol block uses backticks.
        let text = format!("~~~{}\n{}\n~~~\n", AGENT_FENCE_INFO, body);
        assert_eq!(parse_agent_response(&text), Err(ParseError::NoBlock));
    }

    #[test]
    fn indented_agent_fence_is_a_code_sample() {
        // CommonMark: four or more spaces (or a tab) before the fence make
        // an indented code block, i.e. a quoted sample. Never a proposal.
        let body = ls_command();
        for indent in ["    ", "      ", "\t", " \t", "   \t", "\u{a0}"] {
            let text = format!(
                "Example:\n\n{i}```{}\n{i}{}\n{i}```\n",
                AGENT_FENCE_INFO,
                body,
                i = indent
            );
            assert_eq!(
                parse_agent_response(&text),
                Err(ParseError::NoBlock),
                "indent {:?}",
                indent
            );
        }
        for indent in ["", " ", "  ", "   "] {
            let text = format!(
                "1. Step:\n\n{i}```{}\n{i}{}\n{i}```\n",
                AGENT_FENCE_INFO,
                body,
                i = indent
            );
            let parsed = parse_agent_response(&text).unwrap();
            assert_eq!(parsed.response.command.unwrap().command, "ls -la");
        }
    }

    #[test]
    fn longer_backtick_fence_with_the_label_is_accepted() {
        let text = format!("````{}\n{}\n````\n", AGENT_FENCE_INFO, ls_command());
        let parsed = parse_agent_response(&text).unwrap();
        assert_eq!(parsed.response.command.unwrap().command, "ls -la");
    }

    #[test]
    fn repair_prompt_is_nonempty_and_mentions_protocol() {
        assert!(REPAIR_PROMPT.contains("cgpt-agent-response-v1"));
    }

    #[test]
    fn prompt_contract_is_loaded() {
        assert!(AGENT_PROMPT_CONTRACT.contains("cgpt-agent-response-v1"));
        assert!(AGENT_PROMPT_CONTRACT.len() > 200);
    }
}
