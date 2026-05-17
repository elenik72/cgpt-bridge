//! Agent-mode protocol: types and parser for `cgpt-agent-response-v1` and
//! `cgpt-command-result-v1`, plus the prompt contract the CLI prepends on
//! the first turn of `cgpt agent`.
//!
//! The wire spec lives in `docs/protocol.md`. This module is the canonical
//! Rust mirror of that spec. The CLI parses ChatGPT's assistant text with
//! `parse_agent_response`, never with ad-hoc regex elsewhere.

use serde::{Deserialize, Serialize};

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

fn validate_command(cmd: &CommandRequest) -> Result<(), ParseError> {
    if cmd.command.trim().is_empty() {
        return Err(ParseError::InvalidCommand {
            detail: "command string is empty".into(),
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

/// Extract the body of every fenced code block whose info string matches
/// `info` exactly. We only recognise triple-backtick fences (the only kind
/// ChatGPT renders), and the info string must match case-sensitively.
fn extract_fenced_bodies(text: &str, info: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = fence_open(line) {
            if rest == info {
                // Collect until the matching closing fence (any ``` at line start).
                let mut body = String::new();
                let mut closed = false;
                for inner in lines.by_ref() {
                    if is_fence_close(inner) {
                        closed = true;
                        break;
                    }
                    body.push_str(inner);
                    body.push('\n');
                }
                if closed {
                    bodies.push(body);
                }
                // If the fence never closed we drop the partial block — the
                // assistant text was malformed and the caller will see
                // NoBlock or DuplicateBlocks depending on what else is there.
            }
        }
    }
    bodies
}

/// Returns `Some(info)` if the line opens a triple-backtick fence with an
/// info string. Whitespace at line start is allowed, info string is trimmed.
fn fence_open(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.trim();
        if rest.is_empty() {
            None
        } else {
            Some(rest)
        }
    } else {
        None
    }
}

fn is_fence_close(line: &str) -> bool {
    let trimmed = line.trim_start().trim_end();
    trimmed == "```"
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
            minimal_final().to_string()
        );
        let parsed = parse_agent_response(&text).unwrap();
        assert_eq!(parsed.response.status, Status::Final);
    }

    #[test]
    fn fence_extraction_ignores_other_info_strings() {
        let text = format!(
            "```json\n{{}}\n```\n```{}\n{}\n```\n",
            AGENT_FENCE_INFO,
            minimal_final().to_string()
        );
        let parsed = parse_agent_response(&text).unwrap();
        assert_eq!(parsed.response.status, Status::Final);
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
