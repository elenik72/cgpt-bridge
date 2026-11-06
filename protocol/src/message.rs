//! Transport-level request/response envelopes.
//!
//! These are the messages exchanged between local processes (CLI ↔ host ↔
//! Chrome extension). The assistant-side protocol (`cgpt-agent-response-v1`)
//! is a higher layer and is not modeled here.

use serde::{Deserialize, Serialize};

/// Crate-level version string for the host. Useful in `pong` payloads so
/// clients can tell which build they're talking to.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which build this is: `CGPT_BUILD` at compile time (the release scripts set
/// the commit's short SHA), `dev` without it. Every crate of the workspace
/// reports this one value; the extension has its own from the same variable.
pub const BUILD: &str = match option_env!("CGPT_BUILD") {
    Some(build) if !build.is_empty() => build,
    _ => "dev",
};

/// Inbound message: either Chrome→host (Native Messaging) or CLI→host (UDS).
/// The same envelope works on both because the framing is the same.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeRequest {
    /// Health check. No tab interaction.
    Ping(PingRequest),

    /// "Single ask" request: insert a prompt into the active ChatGPT tab and
    /// return the visible assistant response. Sent by `cgpt ask`; will be
    /// forwarded to the Chrome extension in M5 phase 2.
    Ask(AskRequest),

    /// Press "Stop generating" in the addressed ChatGPT tab. The extension
    /// answers with `stopped`.
    Stop(StopRequest),

    /// Stop the one ask `ask_id` names (`cgpt ask` on Ctrl+C), in whatever
    /// tab it runs, and nothing else. The extension answers with `stopped`;
    /// `was_generating: false` when it knows no running ask by that id. A
    /// type of its own, not a field of `stop`: a host or extension built
    /// before it refuses or ignores it rather than stopping another answer.
    StopAsk(StopAskRequest),

    /// Ask the extension what state its ChatGPT tab is in. The extension
    /// answers with `status_result`.
    Status(StatusRequest),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PingRequest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AskRequest {
    /// Caller-generated request id. Echoed back in the response so the host
    /// router can correlate concurrent CLI clients.
    pub id: String,
    /// Prompt text to send to the active ChatGPT tab.
    pub text: String,
    /// Per-request wall-clock timeout, in milliseconds.
    pub timeout_ms: u64,
    /// Which ChatGPT conversation to send to. `None` keeps the original
    /// behavior: the user's active ChatGPT tab, whatever chat it shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ChatTarget>,
    /// The ChatGPT project a NEW chat of this ask goes into (created when
    /// there is none by that name); an ask that continues a chat ignores it.
    /// `None`: wherever ChatGPT puts a new chat. Must pass
    /// [`check_project_name`]. A host built before this field drops it (the
    /// ask then goes to a plain new chat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Debug aid (the gateway's `debug_capture_turns`): the extension
    /// returns the answer's event stream in `ask_result` (`stream_capture`).
    /// Off unless set; a host or extension built before it drops it, and no
    /// capture comes back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub capture_stream: bool,
    /// Files to put into the composer before the text is sent (images from
    /// OpenCode, sent by the gateway). Empty: none, and left out of the
    /// frame. A host or extension built before this field drops it, so the
    /// gateway sends files only to an extension that reports
    /// `attachments: true` in `status_result`. The whole frame, base64
    /// included, must stay within [`MAX_FRAME_BYTES`](crate::MAX_FRAME_BYTES)
    /// (Chrome's 1 MB limit for a message from the host).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// One file of an `ask`. Wire form:
/// `{"name":"image-1.png","mime":"image/png","data_base64":"iVBOR…"}`.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The file name the page shows (made by the sender, never a path).
    pub name: String,
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub mime: String,
    /// The file's bytes, standard base64 with padding.
    pub data_base64: String,
}

/// Longest project name an `ask` may carry, in characters.
pub const MAX_PROJECT_NAME_CHARS: usize = 100;

/// Whether `name` may be sent as an `ask`'s `project`: not empty, at most
/// [`MAX_PROJECT_NAME_CHARS`] characters, no control characters, and no
/// whitespace at either end (the page would not show it, so the project
/// would never be found again by its name).
pub fn check_project_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("project name is empty".to_string());
    }
    if name.trim() != name {
        return Err("project name starts or ends with whitespace".to_string());
    }
    let chars = name.chars().count();
    if chars > MAX_PROJECT_NAME_CHARS {
        return Err(format!(
            "project name is {} characters, more than {}",
            chars, MAX_PROJECT_NAME_CHARS
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("project name holds a control character".to_string());
    }
    Ok(())
}

/// What the extension reports about an answered ask beside its text.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
pub struct AskDiagnostics {
    /// How the answer read from the stream compares with the answer the
    /// page shows; absent when the page showed no settled answer to compare.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_check: Option<StreamCheck>,
    /// The answer's event stream, capped, when the ask set `capture_stream`
    /// (absent otherwise): every event with its text pieces replaced by
    /// `«text OFF+LEN»`, then the pieces joined (extension `sseParser.ts`
    /// `captureEvent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_capture: Option<String>,
    /// The ask set `capture_stream`, but the capture was left out whole:
    /// the frame would have been too big with it (a capture is never cut).
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream_capture_dropped: bool,
}

/// The answer as read from ChatGPT's stream next to the answer the page
/// shows, both normalized the same way (whitespace and markdown syntax
/// dropped, see the extension's `compareStreamAndPage`): their lengths in
/// characters, and whether they differ.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct StreamCheck {
    pub stream_chars: u64,
    pub page_chars: u64,
    pub mismatch: bool,
}

/// Where an `ask` should land. Wire form: `{"kind":"conversation","url":"…"}`
/// or `{"kind":"new_chat"}`.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum ChatTarget {
    /// An existing conversation, addressed by its chatgpt.com URL.
    #[serde(rename = "conversation")]
    Conversation { url: String },
    /// A fresh chat; the extension reports its URL back in `ask_result`.
    #[serde(rename = "new_chat")]
    NewChat,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StopRequest {
    pub id: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StopAskRequest {
    pub id: String,
    pub ask_id: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StatusRequest {
    pub id: String,
}

/// Outbound message back to the caller (extension over NM, or CLI over UDS).
///
/// `host_version` is `Cow<'static, str>` rather than `&'static str` so callers
/// that *deserialize* a response (the CLI) can own the string, while the host
/// can still send a `'static` literal at zero cost.
///
/// `build` (in `pong` the host's [`BUILD`], in `status_result` the
/// extension's) is optional both ways: a component built before it sends
/// none, and one that does not know it ignores it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeResponse {
    Pong {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        host_version: std::borrow::Cow<'static, str>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        echo: Option<serde_json::Value>,
        /// u64, not u128: an internally tagged enum buffers its fields, and
        /// that buffer has no u128, so a `pong` could not be read back.
        ts_unix_ms: u64,
    },

    /// Successful ask: the visible assistant response text.
    /// `conversation_url` is the chat the answer came from; absent when the
    /// extension (or an older one) does not report it. `warning`: something
    /// the caller should show (say, the new chat could not be put into the
    /// asked project and went to a plain new chat); absent when all went as
    /// asked, and always from a component built before it.
    AskResult {
        id: String,
        text: String,
        #[serde(default)]
        conversation_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
        /// What the extension noticed about the answer (see
        /// [`AskDiagnostics`]); absent when there is nothing to say, and
        /// from a component built before it. Only a report: `text` is the
        /// same either way.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostics: Option<AskDiagnostics>,
    },

    /// Answer to `stop` and `stop_ask`. `was_generating` is false when there
    /// was nothing to stop. `other_chat` (`stop_ask` only): nothing was
    /// pressed because the tab shows another chat than the ask's.
    /// `not_sent` (`stop_ask` only): the ask was cancelled before its prompt
    /// was sent (its tab was still being chosen, it was queued behind
    /// another ask, or the page had not sent it yet); it ends `cancelled`.
    Stopped {
        id: String,
        was_generating: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        other_chat: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        not_sent: bool,
    },

    /// Answer to `status`. With `extension: false` the host itself fills in
    /// the other fields (no extension is connected to ask).
    StatusResult {
        id: String,
        extension: bool,
        tab_open: bool,
        logged_in: bool,
        #[serde(default)]
        conversation_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        /// How many own tabs the extension is ready to keep for gateway asks
        /// (several gateway sessions at once). An extension or host that
        /// predates it sends none, and the gateway then runs one turn at a
        /// time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        own_tabs: Option<u32>,
        /// Whether the extension puts an ask's `attachments` into the
        /// composer. An extension or host that predates it sends none (read
        /// as false), and the gateway then sends no files.
        #[serde(default, skip_serializing_if = "is_false")]
        attachments: bool,
    },

    /// Anything that went wrong. Stable, machine-readable `code` field;
    /// human-readable `message`. Mapped to CLI exit codes by the CLI.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        code: ErrorCode,
        message: String,
    },
}

/// Stable machine-readable error codes. Order and string form are part of the
/// wire protocol; adding new variants is fine, renaming existing ones is not.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Inbound JSON was malformed or did not match a known request shape.
    BadRequest,
    /// Inbound frame body exceeded `MAX_FRAME_BYTES`.
    OversizeFrame,
    /// The Chrome extension is not currently connected to the host. This
    /// happens when the user has no Chrome running, the extension is
    /// disabled, or the SW has not yet woken up.
    ExtensionUnavailable,
    /// No tab matching `https://chatgpt.com/*` is the active tab.
    TabUnavailable,
    /// DOM adapter failure inside the page (composer not found, etc.).
    DomFailure,
    /// Wall-clock timeout from the caller's `timeout_ms`.
    Timeout,
    /// ChatGPT is throttling the account (HTTP 429 on the conversation
    /// stream); the caller should back off.
    RateLimited,
    /// The turn was stopped on purpose: the Stop button in the ChatGPT page,
    /// or a `stop` request. Not worth a retry. An extension built before this
    /// code (or one talking to such a host) reports it as `internal` with
    /// the exact messages in tests/fixtures/stop_messages.txt.
    Cancelled,
    /// The tab is busy: an earlier answer is still generating there, or
    /// another ask runs in it. The ask was refused before anything was
    /// typed. An extension built before this code (or one talking to such a
    /// host) reports it as `dom_failure` with the same message.
    TabBusy,
    /// Catch-all for anything the host did not anticipate.
    Internal,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Handle a request that needs no tab interaction (Ping only, for now).
/// `Ask`, `Stop`, `StopAsk` and `Status` are intentionally not handled here — they are
/// routed through the extension and get a real response from the page.
pub fn dispatch_basic(req: BridgeRequest, ts_unix_ms: u64) -> Option<BridgeResponse> {
    match req {
        BridgeRequest::Ping(p) => Some(BridgeResponse::Pong {
            id: p.id,
            host_version: std::borrow::Cow::Borrowed(HOST_VERSION),
            build: Some(BUILD.to_string()),
            echo: p.payload,
            ts_unix_ms,
        }),
        BridgeRequest::Ask(_)
        | BridgeRequest::Stop(_)
        | BridgeRequest::StopAsk(_)
        | BridgeRequest::Status(_) => None,
    }
}

pub fn error_response(
    id: Option<String>,
    code: ErrorCode,
    message: impl Into<String>,
) -> BridgeResponse {
    BridgeResponse::Error {
        id,
        code,
        message: message.into(),
    }
}

/// Extract the correlation id from any `BridgeResponse`. Returns `None` only
/// when the response is an Error or Pong with no id (legitimate for stray
/// host-generated errors that have no caller to correlate against).
pub fn response_id(resp: &BridgeResponse) -> Option<&str> {
    match resp {
        BridgeResponse::Pong { id, .. } => id.as_deref(),
        BridgeResponse::AskResult { id, .. }
        | BridgeResponse::Stopped { id, .. }
        | BridgeResponse::StatusResult { id, .. } => Some(id),
        BridgeResponse::Error { id, .. } => id.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_ping() {
        let req: BridgeRequest = serde_json::from_str(r#"{"type":"ping","id":"x1"}"#).unwrap();
        let resp = dispatch_basic(req, 1234).unwrap();
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""type":"pong""#));
        assert!(s.contains(r#""id":"x1""#));
        assert!(s.contains(r#""ts_unix_ms":1234"#));
    }

    #[test]
    fn dispatch_ask_returns_none() {
        let req: BridgeRequest =
            serde_json::from_str(r#"{"type":"ask","id":"a1","text":"hi","timeout_ms":1000}"#)
                .unwrap();
        assert!(dispatch_basic(req, 0).is_none());
    }

    #[test]
    fn ping_echoes_payload() {
        let req: BridgeRequest =
            serde_json::from_str(r#"{"type":"ping","id":"x2","payload":{"a":1}}"#).unwrap();
        let resp = dispatch_basic(req, 0).unwrap();
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""echo":{"a":1}"#));
    }

    #[test]
    fn rejects_unknown_type() {
        let r: Result<BridgeRequest, _> = serde_json::from_str(r#"{"type":"explode"}"#);
        assert!(r.is_err());
    }

    #[test]
    fn error_serializes_snake_case_code() {
        let resp = error_response(
            Some("a1".into()),
            ErrorCode::TabUnavailable,
            "no chatgpt tab",
        );
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""code":"tab_unavailable""#));
        assert!(s.contains(r#""message":"no chatgpt tab""#));
        assert!(s.contains(r#""id":"a1""#));
    }

    #[test]
    fn ask_with_target_roundtrip() {
        let conv = AskRequest {
            id: "a1".into(),
            text: "hi".into(),
            timeout_ms: 1000,
            target: Some(ChatTarget::Conversation {
                url: "https://chatgpt.com/c/abc".into(),
            }),
            project: None,
            capture_stream: false,
            attachments: Vec::new(),
        };
        let s = serde_json::to_string(&BridgeRequest::Ask(conv)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "ask");
        assert_eq!(v["target"]["kind"], "conversation");
        assert_eq!(v["target"]["url"], "https://chatgpt.com/c/abc");
        match serde_json::from_str::<BridgeRequest>(&s).unwrap() {
            BridgeRequest::Ask(a) => assert_eq!(
                a.target,
                Some(ChatTarget::Conversation {
                    url: "https://chatgpt.com/c/abc".into()
                })
            ),
            other => panic!("unexpected: {:?}", other),
        }

        let new_chat = AskRequest {
            id: "a2".into(),
            text: "hi".into(),
            timeout_ms: 1000,
            target: Some(ChatTarget::NewChat),
            project: None,
            capture_stream: false,
            attachments: Vec::new(),
        };
        let s = serde_json::to_string(&BridgeRequest::Ask(new_chat)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["target"], serde_json::json!({ "kind": "new_chat" }));
        match serde_json::from_str::<BridgeRequest>(&s).unwrap() {
            BridgeRequest::Ask(a) => assert_eq!(a.target, Some(ChatTarget::NewChat)),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn ask_without_target_is_backward_compatible() {
        let req: BridgeRequest =
            serde_json::from_str(r#"{"type":"ask","id":"a1","text":"hi","timeout_ms":1000}"#)
                .unwrap();
        let BridgeRequest::Ask(a) = req else {
            panic!("expected ask");
        };
        assert_eq!(a.target, None);
        // And an ask without a target serializes without the field, so the
        // extension sees exactly what the CLI sent.
        let s = serde_json::to_string(&BridgeRequest::Ask(a)).unwrap();
        assert!(!s.contains("target"));
    }

    #[test]
    fn ask_project_roundtrip_and_absent_when_none() {
        let req: BridgeRequest = serde_json::from_str(
            r#"{"type":"ask","id":"a1","text":"hi","timeout_ms":1000,"target":{"kind":"new_chat"},"project":"cgpt-bridge"}"#,
        )
        .unwrap();
        let BridgeRequest::Ask(a) = req else {
            panic!("expected ask");
        };
        assert_eq!(a.project.as_deref(), Some("cgpt-bridge"));
        let s = serde_json::to_string(&BridgeRequest::Ask(a.clone())).unwrap();
        assert!(s.contains(r#""project":"cgpt-bridge""#), "{s}");
        let none = AskRequest { project: None, ..a };
        let s = serde_json::to_string(&BridgeRequest::Ask(none)).unwrap();
        assert!(!s.contains("project"), "{s}");
    }

    #[test]
    fn project_names_are_checked() {
        for ok in [
            "cgpt-bridge",
            "мой проект",
            "a",
            &"я".repeat(MAX_PROJECT_NAME_CHARS),
        ] {
            assert_eq!(check_project_name(ok), Ok(()), "{ok:?}");
        }
        let too_long = "я".repeat(MAX_PROJECT_NAME_CHARS + 1);
        for bad in [
            "",
            "   ",
            " lead",
            "trail ",
            "a\nb",
            "a\u{1b}[2Jb",
            "tab\there",
            &too_long,
        ] {
            assert!(check_project_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_dropped_capture_still_parses_and_says_so() {
        // What the extension's capHostFrame sends when the frame would be too
        // big with the capture (extension/test/frameLimit.test.ts checks the
        // same JSON): the answer whole, the capture left out, never cut.
        let dropped = r#"{"type":"ask_result","id":"c9","text":"A","diagnostics":{"stream_capture_dropped":true}}"#;
        let resp: BridgeResponse = serde_json::from_str(dropped).unwrap();
        match &resp {
            BridgeResponse::AskResult {
                text, diagnostics, ..
            } => {
                assert_eq!(text, "A");
                let d = diagnostics.as_ref().unwrap();
                assert!(d.stream_capture_dropped);
                assert_eq!(d.stream_capture, None);
            }
            other => panic!("{:?}", other),
        }
        // Passed on unchanged by the host; false is left out.
        assert_eq!(
            serde_json::to_value(&resp).unwrap()["diagnostics"],
            serde_json::json!({"stream_capture_dropped": true})
        );
        let plain = serde_json::to_value(AskDiagnostics::default()).unwrap();
        assert_eq!(plain, serde_json::json!({}));
    }

    #[test]
    fn ask_result_warning_optional() {
        let old: BridgeResponse =
            serde_json::from_str(r#"{"type":"ask_result","id":"a1","text":"ok"}"#).unwrap();
        let s = serde_json::to_string(&old).unwrap();
        assert!(!s.contains("warning"), "{s}");
        let new: BridgeResponse = serde_json::from_str(
            r#"{"type":"ask_result","id":"a1","text":"ok","warning":"not in the project"}"#,
        )
        .unwrap();
        match new {
            BridgeResponse::AskResult { warning, .. } => {
                assert_eq!(warning.as_deref(), Some("not in the project"))
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn ask_result_conversation_url_optional() {
        let old: BridgeResponse =
            serde_json::from_str(r#"{"type":"ask_result","id":"a1","text":"ok"}"#).unwrap();
        match old {
            BridgeResponse::AskResult {
                conversation_url, ..
            } => assert_eq!(conversation_url, None),
            other => panic!("unexpected: {:?}", other),
        }
        let new: BridgeResponse = serde_json::from_str(
            r#"{"type":"ask_result","id":"a1","text":"ok","conversation_url":"https://chatgpt.com/c/x"}"#,
        )
        .unwrap();
        match new {
            BridgeResponse::AskResult {
                conversation_url, ..
            } => assert_eq!(conversation_url.as_deref(), Some("https://chatgpt.com/c/x")),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn stop_and_status_roundtrip() {
        let stop: BridgeRequest = serde_json::from_str(r#"{"type":"stop","id":"s1"}"#).unwrap();
        let s = serde_json::to_string(&stop).unwrap();
        assert_eq!(s, r#"{"type":"stop","id":"s1"}"#);
        let status: BridgeRequest = serde_json::from_str(r#"{"type":"status","id":"t1"}"#).unwrap();
        let s = serde_json::to_string(&status).unwrap();
        assert_eq!(s, r#"{"type":"status","id":"t1"}"#);
        assert!(dispatch_basic(stop, 0).is_none());
        assert!(dispatch_basic(status, 0).is_none());

        let stopped: BridgeResponse =
            serde_json::from_str(r#"{"type":"stopped","id":"s1","was_generating":true}"#).unwrap();
        assert_eq!(response_id(&stopped), Some("s1"));
        assert!(matches!(
            stopped,
            BridgeResponse::Stopped {
                was_generating: true,
                other_chat: false,
                not_sent: false,
                ..
            }
        ));
        // Left out unless set, so a `stopped` reads as before.
        assert_eq!(
            serde_json::to_value(&stopped).unwrap(),
            serde_json::json!({"type":"stopped","id":"s1","was_generating":true})
        );

        let result: BridgeResponse = serde_json::from_str(
            r#"{"type":"status_result","id":"t1","extension":true,"tab_open":true,"logged_in":false,"conversation_url":null}"#,
        )
        .unwrap();
        assert_eq!(response_id(&result), Some("t1"));
        let s = serde_json::to_string(&result).unwrap();
        assert!(s.contains(r#""type":"status_result""#));
        assert!(s.contains(r#""logged_in":false"#));
        assert!(matches!(
            result,
            BridgeResponse::StatusResult {
                extension: true,
                tab_open: true,
                logged_in: false,
                conversation_url: None,
                build: None,
                own_tabs: None,
                ..
            }
        ));
    }

    #[test]
    fn status_result_own_tabs_round_trips_and_is_optional() {
        let with: BridgeResponse = serde_json::from_str(
            r#"{"type":"status_result","id":"t2","extension":true,"tab_open":true,"logged_in":true,"conversation_url":null,"own_tabs":8}"#,
        )
        .unwrap();
        assert!(matches!(
            with,
            BridgeResponse::StatusResult {
                own_tabs: Some(8),
                ..
            }
        ));
        let text = serde_json::to_string(&with).unwrap();
        assert!(text.contains(r#""own_tabs":8"#), "{}", text);
        let back: BridgeResponse = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            back,
            BridgeResponse::StatusResult {
                own_tabs: Some(8),
                ..
            }
        ));

        // Without the field (an older extension, or the host's own answer
        // when no extension is connected): none, and none written back.
        let without = BridgeResponse::StatusResult {
            id: "t3".into(),
            extension: false,
            tab_open: false,
            logged_in: false,
            conversation_url: None,
            build: None,
            own_tabs: None,
            attachments: false,
        };
        let text = serde_json::to_string(&without).unwrap();
        assert!(!text.contains("own_tabs"), "{}", text);
        let back: BridgeResponse = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            back,
            BridgeResponse::StatusResult { own_tabs: None, .. }
        ));
    }

    #[test]
    fn ask_attachments_round_trip_and_are_left_out_when_empty() {
        let req: BridgeRequest = serde_json::from_str(
            r#"{"type":"ask","id":"a1","text":"hi","timeout_ms":1000,"attachments":[{"name":"image-1.png","mime":"image/png","data_base64":"iVBORw0KGgo="}]}"#,
        )
        .unwrap();
        let BridgeRequest::Ask(a) = req else {
            panic!("expected ask");
        };
        assert_eq!(
            a.attachments,
            vec![Attachment {
                name: "image-1.png".into(),
                mime: "image/png".into(),
                data_base64: "iVBORw0KGgo=".into(),
            }]
        );
        let s = serde_json::to_string(&BridgeRequest::Ask(a.clone())).unwrap();
        assert!(s.contains(r#""data_base64":"iVBORw0KGgo=""#), "{s}");
        // Without files the frame is the one from before the field.
        let none = AskRequest {
            attachments: Vec::new(),
            ..a
        };
        let s = serde_json::to_string(&BridgeRequest::Ask(none)).unwrap();
        assert!(!s.contains("attachments"), "{s}");
    }

    #[test]
    fn status_result_attachments_is_optional_and_left_out_when_false() {
        let with: BridgeResponse = serde_json::from_str(
            r#"{"type":"status_result","id":"t2","extension":true,"tab_open":true,"logged_in":true,"conversation_url":null,"own_tabs":8,"attachments":true}"#,
        )
        .unwrap();
        assert!(matches!(
            with,
            BridgeResponse::StatusResult {
                attachments: true,
                ..
            }
        ));
        let text = serde_json::to_string(&with).unwrap();
        assert!(text.contains(r#""attachments":true"#), "{text}");
        // An older extension (or the host's own answer): false, left out.
        let old: BridgeResponse = serde_json::from_str(
            r#"{"type":"status_result","id":"t3","extension":true,"tab_open":true,"logged_in":true,"conversation_url":null}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            BridgeResponse::StatusResult {
                attachments: false,
                ..
            }
        ));
        let text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("attachments"), "{text}");
    }

    #[test]
    fn rate_limited_code_snake_case() {
        let resp = error_response(Some("a1".into()), ErrorCode::RateLimited, "slow down");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""code":"rate_limited""#));
        let back: BridgeResponse = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            BridgeResponse::Error {
                code: ErrorCode::RateLimited,
                ..
            }
        ));
    }
}
