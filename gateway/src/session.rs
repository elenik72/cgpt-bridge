//! Sessions: which ChatGPT chat belongs to which OpenCode session, how much
//! of its history that chat has already seen, and what the next turn has to
//! send. One JSON file per session in `<data_dir>/cgpt-gateway/sessions/`;
//! sessions unused for 7 days are deleted.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::log::log;
use crate::openai::{ChatRequest, Message};
use crate::safety;

/// The header OpenCode sends with every request of a session, subagents
/// included.
pub const SESSION_HEADER: &str = "x-opencode-session-id";
/// Sessions without a turn for this long are deleted.
pub const MAX_IDLE_MS: u128 = 7 * 24 * 60 * 60 * 1000;
/// A header value longer than this, or with other characters than ASCII
/// letters, digits, `-` and `_`, is not used as a file name as it is.
const MAX_KEY_CHARS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    /// A turn is running, or the gateway stopped in the middle of one.
    InFlight,
    /// The last turn was cut off: an error, a timeout or the client left.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub key: String,
    pub chat_url: Option<String>,
    /// Fingerprints of the non-system messages of the last answered request:
    /// the history the chat has seen. Empty until a bootstrap is answered.
    pub delivered: Vec<u64>,
    /// Where the delta of the last answered request started (0 after a
    /// bootstrap); a repeat of that request sends the same delta again.
    pub turn_from: usize,
    pub system_hash: u64,
    pub tools_hash: u64,
    pub state: SessionState,
    pub last_used_ms: u128,
    /// The number of the last tool call handed out in this session.
    pub call_seq: u32,
    /// The answered start of a delta cut off between its parts, so that the
    /// chat is sent only the rest of it.
    #[serde(default)]
    pub cut: Option<Cut>,
}

/// The start of a delta's text (as sent: redacted, see `prompt::delta_text`)
/// that the chat has answered: by its length and hash, never the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cut {
    /// Where the delta starts in the history (`Plan::Delta`'s `from`).
    pub from: usize,
    /// Whether the delta opened with the interrupted note.
    pub note: bool,
    /// How many of the delta's parts the chat has answered (the rest is
    /// numbered on from them).
    pub parts: usize,
    /// The answered start's length in bytes and its FNV-1a.
    pub len: usize,
    pub hash: u64,
}

impl Cut {
    pub fn new(from: usize, note: bool, parts: usize, answered: &str) -> Cut {
        Cut {
            from,
            note,
            parts,
            len: answered.len(),
            hash: fnv1a(answered.as_bytes()),
        }
    }

    /// What `text` has after the answered start, when it begins with it and
    /// goes on.
    pub fn rest<'a>(&self, text: &'a str) -> Option<&'a str> {
        let start = text.get(..self.len)?;
        let rest = &text[self.len..];
        (fnv1a(start.as_bytes()) == self.hash && !rest.is_empty()).then_some(rest)
    }
}

impl Session {
    pub fn new(key: &str) -> Session {
        Session {
            key: key.to_string(),
            chat_url: None,
            delivered: Vec::new(),
            turn_from: 0,
            system_hash: 0,
            tools_hash: 0,
            state: SessionState::Idle,
            last_used_ms: 0,
            call_seq: 0,
            cut: None,
        }
    }

    /// The 8 hex digits of `call_<8 hex>_<n>` tool-call ids.
    pub fn call_prefix(&self) -> String {
        format!("{:08x}", fnv1a(self.key.as_bytes()) as u32)
    }
}

/// What the next turn sends to the session's chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// A new chat with the bootstrap pack.
    Bootstrap,
    /// The same chat, the non-system messages from `from` on. The note says
    /// the previous message was cut off; `updated_rules` sends the new system
    /// prompt and tools before the delta.
    Delta {
        from: usize,
        note_interrupted: bool,
        updated_rules: bool,
    },
}

/// - no session, no chat yet, or a history that does not start with what
///   the chat has seen (compacted, edited, another task): a new chat;
/// - otherwise the messages after the seen ones go to the same chat, after
///   the note when the last turn was cut off or the history does not hold
///   our answer to it;
/// - the very request answered last, again (its answer was lost on the
///   way): its delta again, after the note; a repeated bootstrap is a new
///   bootstrap.
pub fn plan_turn(s: Option<&Session>, req: &ChatRequest, allow: &[String]) -> Plan {
    let s = match s {
        Some(s) if s.chat_url.is_some() && !s.delivered.is_empty() => s,
        _ => return Plan::Bootstrap,
    };
    let history = history(req);
    let seen: Vec<u64> = history.iter().map(fingerprint).collect();
    let n = s.delivered.len();
    if seen.len() < n || seen[..n] != s.delivered[..] {
        return Plan::Bootstrap;
    }
    let updated_rules = system_hash(req) != s.system_hash || tools_hash(req, allow) != s.tools_hash;
    if seen.len() == n {
        if s.turn_from == 0 {
            // The answered request was the bootstrap: resent as a delta it
            // would be the whole history, unclipped.
            return Plan::Bootstrap;
        }
        return Plan::Delta {
            from: s.turn_from.min(n),
            note_interrupted: true,
            updated_rules,
        };
    }
    // The message after the seen ones is our answer when the client got it.
    // When it is not (the client left in the last seconds of the turn, which
    // the gateway counted as delivered), the chat has an answer OpenCode
    // never saw.
    let answer_missing = !history.get(n).is_some_and(|m| m.role == "assistant");
    Plan::Delta {
        from: n,
        // `InFlight` on load: the gateway stopped in the middle of a turn.
        note_interrupted: s.state != SessionState::Idle || answer_missing,
        updated_rules,
    }
}

/// FNV-1a, 64 bit.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The fingerprint of one message: FNV-1a of its canonical JSON — role,
/// text, tool calls by name and arguments (not by id), and `tool_call_id`.
pub fn fingerprint(m: &Message) -> u64 {
    let calls: Vec<Value> = m
        .tool_calls
        .iter()
        .flatten()
        .map(|c| {
            // Parsed, so that key order and spacing do not matter.
            let args = serde_json::from_str::<Value>(&c.function.arguments)
                .unwrap_or_else(|_| Value::String(c.function.arguments.clone()));
            json!({ "name": c.function.name, "arguments": args })
        })
        .collect();
    let canonical = json!({
        "role": m.role,
        "content": m.text(),
        "tool_calls": calls,
        "tool_call_id": m.tool_call_id,
    });
    fnv1a(canonical.to_string().as_bytes())
}

/// The non-system messages of a request, in order.
pub fn history(req: &ChatRequest) -> Vec<Message> {
    req.messages
        .iter()
        .filter(|m| m.role != "system")
        .cloned()
        .collect()
}

/// OpenCode's system prompt: the non-empty system messages joined.
pub fn system_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.text())
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn system_hash(req: &ChatRequest) -> u64 {
    fnv1a(system_text(req).as_bytes())
}

/// A hash of the tools ChatGPT is shown (`allow` is the config's
/// `allow_tools`): a change of either is news for the chat.
pub fn tools_hash(req: &ChatRequest, allow: &[String]) -> u64 {
    let shown = safety::visible_tools(req.tools.as_deref().unwrap_or(&[]), allow);
    let tools = serde_json::to_string(&shown).unwrap_or_default();
    fnv1a(tools.as_bytes())
}

fn safe_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_KEY_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub struct Sessions {
    dir: PathBuf,
}

impl Sessions {
    pub fn new(dir: PathBuf) -> Sessions {
        Sessions { dir }
    }

    /// `x-opencode-session-id` when present (hashed when it is not a safe
    /// file name), else a hash of the first user message.
    pub fn key_for(headers: &[(String, String)], req: &ChatRequest) -> String {
        let header = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(SESSION_HEADER))
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty());
        match header {
            Some(v) if safe_key(v) => v.to_string(),
            Some(v) => format!("h{:016x}", fnv1a(v.as_bytes())),
            None => {
                let first = req.messages.iter().find(|m| m.role == "user");
                format!("u{:016x}", first.map(fingerprint).unwrap_or(0))
            }
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.json", key))
    }

    /// The saved session, or `None` when there is none or it cannot be read
    /// (the next turn then starts a new chat and overwrites it).
    pub fn load(&self, key: &str) -> Option<Session> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Write the session file whole: a temporary file renamed over it.
    pub fn save(&self, s: &Session) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{}.json.tmp", s.key));
        std::fs::write(&tmp, serde_json::to_vec(s)?)?;
        std::fs::rename(&tmp, self.path(&s.key))
    }

    /// Delete the sessions without a turn for more than 7 days, and
    /// temporary files of saves that never finished, as old; returns how
    /// many files were deleted. Files that are not ours are left alone, and
    /// one file that cannot be deleted does not stop the rest.
    pub fn expire(&self, now_ms: u128) -> io::Result<usize> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let stale = |last_used_ms: u128| now_ms.saturating_sub(last_used_ms) > MAX_IDLE_MS;
        let mut deleted = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let expired = if name.ends_with(".json.tmp") {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .is_some_and(|t| stale(t.as_millis()))
            } else if name.ends_with(".json") {
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<Session>(&t).ok())
                    .is_some_and(|s| stale(s.last_used_ms))
            } else {
                false
            };
            if !expired {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => deleted += 1,
                Err(e) => log(format_args!(
                    "cgpt-gateway: {} not deleted: {}",
                    path.display(),
                    e
                )),
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::openai::{Content, FunctionCall, Part, Tool, ToolCall, ToolFunction};

    /// A fresh directory under the system temp dir, unique in this process.
    pub fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cgpt-gw-sess-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    pub fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(Content::Text(text.into())),
            ..Default::default()
        }
    }

    pub fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "chatgpt-web".into(),
            messages,
            tools: None,
            stream: None,
        }
    }

    fn header(value: &str) -> Vec<(String, String)> {
        vec![("X-OpenCode-Session-Id".to_string(), value.to_string())]
    }

    #[test]
    fn session_key_from_header_else_first_user_hash() {
        let first = request(vec![msg("system", "s"), msg("user", "прочитай a.txt")]);
        let later = request(vec![
            msg("system", "другая подсказка"),
            msg("user", "прочитай a.txt"),
            msg("assistant", "готово"),
            msg("user", "ещё"),
        ]);
        let other = request(vec![msg("user", "прочитай b.txt")]);

        // The header wins, whatever its case, and whatever the history.
        assert_eq!(Sessions::key_for(&header("ses_1"), &first), "ses_1");
        assert_eq!(
            Sessions::key_for(&[("x-opencode-session-id".into(), "ses_1".into())], &other),
            "ses_1"
        );

        // Without it: the first user message decides.
        let k1 = Sessions::key_for(&[], &first);
        assert_eq!(k1, Sessions::key_for(&[], &later));
        assert_ne!(k1, Sessions::key_for(&[], &other));
        assert_ne!(k1, "ses_1");

        // Every key is a safe file name.
        for key in [
            k1,
            Sessions::key_for(&header("../../etc/passwd"), &first),
            Sessions::key_for(&header(&"x".repeat(500)), &first),
            Sessions::key_for(&header("сессия 1"), &first),
        ] {
            assert!(!key.is_empty() && key.len() <= MAX_KEY_CHARS, "{}", key);
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{}",
                key
            );
        }
        assert_ne!(
            Sessions::key_for(&header("../a"), &first),
            Sessions::key_for(&header("../b"), &first)
        );
    }

    #[test]
    fn fingerprint_ignores_call_ids_and_content_shape() {
        let call = |id: &str, args: &str| Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: args.into(),
                },
            }]),
            ..Default::default()
        };
        // Ids and JSON formatting of the arguments do not matter.
        assert_eq!(
            fingerprint(&call("call_1", r#"{"path":"a","limit":5}"#)),
            fingerprint(&call("other", r#"{ "limit": 5, "path": "a" }"#))
        );
        assert_ne!(
            fingerprint(&call("call_1", r#"{"path":"a"}"#)),
            fingerprint(&call("call_1", r#"{"path":"b"}"#))
        );
        // A string and the same text as parts are the same message.
        let parts = Message {
            role: "user".into(),
            content: Some(Content::Parts(vec![Part {
                kind: "text".into(),
                text: Some("привет".into()),
                ..Default::default()
            }])),
            ..Default::default()
        };
        assert_eq!(fingerprint(&parts), fingerprint(&msg("user", "привет")));
        assert_ne!(
            fingerprint(&msg("user", "привет")),
            fingerprint(&msg("user", "привет!"))
        );
        assert_ne!(
            fingerprint(&msg("user", "привет")),
            fingerprint(&msg("assistant", "привет"))
        );
        // The result's call id is part of a tool message.
        let result = |id: &str| Message {
            tool_call_id: Some(id.into()),
            ..msg("tool", "ok")
        };
        assert_ne!(
            fingerprint(&result("call_1")),
            fingerprint(&result("call_2"))
        );
        // FNV-1a 64 reference values.
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn hashes_follow_system_prompt_and_tools() {
        let tool = |name: &str| Tool {
            kind: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: None,
                parameters: json!({"type": "object"}),
            },
        };
        let mut a = request(vec![msg("system", "s1"), msg("user", "u")]);
        let b = request(vec![
            msg("system", "s1"),
            msg("user", "u"),
            msg("assistant", "x"),
        ]);
        assert_eq!(system_hash(&a), system_hash(&b));
        assert_eq!(system_text(&a), "s1");
        let c = request(vec![msg("system", "s2"), msg("user", "u")]);
        assert_ne!(system_hash(&a), system_hash(&c));
        let before = tools_hash(&a, &[]);
        a.tools = Some(vec![tool("read")]);
        assert_ne!(before, tools_hash(&a, &[]));
        let read = tools_hash(&a, &[]);
        a.tools = Some(vec![tool("glob")]);
        assert_ne!(read, tools_hash(&a, &[]));
        // Over what ChatGPT is shown: a hidden `execute` changes nothing,
        // allowing it does.
        let glob = tools_hash(&a, &[]);
        a.tools = Some(vec![tool("glob"), tool("execute")]);
        assert_eq!(glob, tools_hash(&a, &[]));
        assert_ne!(glob, tools_hash(&a, &["execute".to_string()]));
        assert_eq!(history(&b).len(), 2);
    }

    #[test]
    fn a_session_file_without_a_cut_loads_and_a_cut_round_trips() {
        let dir = temp_dir("cut");
        let store = Sessions::new(dir.clone());
        // As an older gateway wrote it.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ses_old.json"),
            r#"{"key":"ses_old","chat_url":"https://chatgpt.com/c/old","delivered":[1],
               "turn_from":0,"system_hash":2,"tools_hash":3,"state":"interrupted",
               "last_used_ms":4,"call_seq":5}"#,
        )
        .unwrap();
        let old = store.load("ses_old").expect("loads");
        assert_eq!(old.cut, None);
        assert_eq!(old.call_seq, 5);

        let mut s = Session::new("ses_cut");
        s.cut = Some(Cut::new(3, true, 1, "начало"));
        store.save(&s).unwrap();
        assert_eq!(store.load("ses_cut"), Some(s));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cut_gives_the_rest_only_of_a_text_that_starts_with_its_start() {
        let cut = Cut::new(0, false, 1, "часть один|");
        assert_eq!(cut.rest("часть один|часть два"), Some("часть два"));
        assert_eq!(cut.rest("часть одиН|часть два"), None, "another start");
        assert_eq!(cut.rest("часть один|"), None, "nothing goes on");
        assert_eq!(cut.rest("часть"), None, "shorter");
        // A length inside a character is no start.
        let mid = Cut {
            len: "ч".len() - 1,
            ..Cut::new(0, false, 1, "ч")
        };
        assert_eq!(mid.rest("часть"), None);
    }

    #[test]
    fn sessions_persist_and_expire_after_7_days() {
        let dir = temp_dir("expire");
        let now: u128 = 1_800_000_000_000;
        let day: u128 = 24 * 60 * 60 * 1000;
        let store = Sessions::new(dir.clone());
        assert_eq!(store.load("nothing"), None);
        assert_eq!(store.expire(now).unwrap(), 0, "no directory yet");

        let mut fresh = Session::new("ses_fresh");
        fresh.chat_url = Some("https://chatgpt.com/c/fresh".into());
        fresh.delivered = vec![1, 2, u64::MAX];
        fresh.state = SessionState::Interrupted;
        fresh.call_seq = 7;
        fresh.last_used_ms = now - 6 * day;
        let mut old = Session::new("ses_old");
        old.last_used_ms = now - 8 * day;
        store.save(&fresh).unwrap();
        store.save(&old).unwrap();

        // A new store on the same directory (a restart) sees both.
        let reopened = Sessions::new(dir.clone());
        assert_eq!(reopened.load("ses_fresh"), Some(fresh.clone()));
        assert_eq!(reopened.load("ses_old"), Some(old.clone()));

        assert_eq!(reopened.expire(now).unwrap(), 1);
        assert_eq!(reopened.load("ses_old"), None);
        assert_eq!(reopened.load("ses_fresh"), Some(fresh.clone()));

        // Saving again replaces the file.
        fresh.call_seq = 8;
        reopened.save(&fresh).unwrap();
        assert_eq!(store.load("ses_fresh").unwrap().call_seq, 8);
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "no temporary files left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_session_header_falls_back_to_first_user_hash() {
        let req = request(vec![msg("user", "прочитай a.txt")]);
        let absent = Sessions::key_for(&[], &req);
        assert_eq!(Sessions::key_for(&header(""), &req), absent);
        assert_eq!(Sessions::key_for(&header("  \t"), &req), absent);
    }

    #[test]
    fn expire_removes_stale_temporary_files() {
        let dir = temp_dir("expire-tmp");
        let store = Sessions::new(dir.clone());
        let mut s = Session::new("ses_live");
        s.last_used_ms = now_ms();
        store.save(&s).unwrap();
        // Left behind by a save that died between write and rename.
        let tmp = dir.join("ses_dead.json.tmp");
        std::fs::write(&tmp, b"{").unwrap();
        let other = dir.join("notes.txt");
        std::fs::write(&other, b"not ours").unwrap();

        assert_eq!(store.expire(now_ms()).unwrap(), 0);
        assert!(
            tmp.exists(),
            "a fresh temporary file may belong to a save in progress"
        );
        let later = now_ms() + MAX_IDLE_MS + 60_000;
        assert_eq!(
            store.expire(later).unwrap(),
            2,
            "the session and the temporary file"
        );
        assert!(!tmp.exists());
        assert!(other.exists(), "files that are not ours stay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session that has answered `answered`, as the engine saves it.
    fn answered(answered: &ChatRequest, turn_from: usize) -> Session {
        let mut s = Session::new("ses_plan");
        s.chat_url = Some("https://chatgpt.com/c/plan".into());
        s.delivered = history(answered).iter().map(fingerprint).collect();
        s.turn_from = turn_from;
        s.system_hash = system_hash(answered);
        s.tools_hash = tools_hash(answered, &[]);
        s
    }

    fn delta_note(plan: Plan) -> (usize, bool) {
        match plan {
            Plan::Delta {
                from,
                note_interrupted,
                ..
            } => (from, note_interrupted),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn next_turn_without_our_answer_in_history_has_the_note() {
        let sent = request(vec![msg("system", "s"), msg("user", "привет")]);
        let s = answered(&sent, 0);

        // The client left after the answer was ready: the new history is
        // the seen messages and a new user message, without our answer.
        let lost = request(vec![
            msg("system", "s"),
            msg("user", "привет"),
            msg("user", "ты тут?"),
        ]);
        assert_eq!(delta_note(plan_turn(Some(&s), &lost, &[])), (1, true));

        // The answer is in the history (text or a tool result after calls):
        // an ordinary continuation.
        let text = request(vec![
            msg("system", "s"),
            msg("user", "привет"),
            msg("assistant", "здравствуй"),
            msg("user", "ты тут?"),
        ]);
        assert_eq!(delta_note(plan_turn(Some(&s), &text, &[])), (1, false));
        let calls = request(vec![
            msg("system", "s"),
            msg("user", "привет"),
            msg("assistant", "читаю"),
            msg("tool", "содержимое"),
        ]);
        assert_eq!(delta_note(plan_turn(Some(&s), &calls, &[])), (1, false));

        // A tool result right after the seen messages is not our answer
        // either.
        let orphan = request(vec![
            msg("system", "s"),
            msg("user", "привет"),
            msg("tool", "содержимое"),
        ]);
        assert_eq!(delta_note(plan_turn(Some(&s), &orphan, &[])), (1, true));

        // An interrupted or in-flight session keeps its note whatever the
        // history holds.
        let mut cut = s.clone();
        cut.state = SessionState::Interrupted;
        assert_eq!(delta_note(plan_turn(Some(&cut), &text, &[])), (1, true));
    }

    #[test]
    fn call_prefix_is_8_hex_per_session() {
        let a = Session::new("ses_a").call_prefix();
        let b = Session::new("ses_b").call_prefix();
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(a, Session::new("ses_a").call_prefix());
    }
}
