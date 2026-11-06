//! Shared wire-format fixtures (`tests/fixtures/*.json`).
//!
//! The extension's tests (`extension/test/wireFixtures.test.ts`) check the
//! frames it builds against the same files, so a field renamed on one side
//! fails a test instead of timing out a live request.
//!
//! Naming: `request_<type>[_<variant>].json` parses as `BridgeRequest`,
//! `response_<type>.json` and `response_error_<code>.json` as
//! `BridgeResponse`. Every fixture must serialize back to the same JSON.
//!
//! `legacy/` holds frames as components built before TASK-043 send them
//! (no `build`): a new reader must still take them, and pass them on
//! unchanged.

use std::collections::BTreeSet;
use std::path::PathBuf;

use cgpt_bridge_protocol::{
    dispatch_basic, BridgeRequest, BridgeResponse, ChatTarget, ErrorCode, BUILD, HOST_VERSION,
};
use serde::Deserialize;
use serde_json::Value;

/// `(file stem, file text, parsed JSON)` of `tests/fixtures/<sub>`. Frames are
/// deserialized from the text, as on the wire.
fn fixtures_in(sub: &str) -> Vec<(String, String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(sub);
    let mut out: Vec<(String, String, Value)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .map(|p| {
            let name = p.file_stem().unwrap().to_str().unwrap().to_owned();
            let text = std::fs::read_to_string(&p).unwrap();
            let value =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: not JSON: {e}"));
            (name, text, value)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn fixtures() -> Vec<(String, String, Value)> {
    fixtures_in("")
}

// The exhaustive matches below fail to compile when a variant is added, as
// a reminder to add its fixture (and the name to the expected set).

fn request_variant(req: &BridgeRequest) -> String {
    match req {
        BridgeRequest::Ping(_) => "ping".into(),
        BridgeRequest::Ask(a) => {
            let target = match &a.target {
                None => "ask",
                Some(ChatTarget::Conversation { .. }) => "ask_conversation",
                Some(ChatTarget::NewChat) => "ask_new_chat",
            };
            let project = match a.project {
                None => target.to_string(),
                Some(_) => format!("{target}_project"),
            };
            let attachments = if a.attachments.is_empty() {
                project
            } else {
                format!("{project}_attachments")
            };
            if a.capture_stream {
                format!("{attachments}_capture")
            } else {
                attachments
            }
        }
        BridgeRequest::Stop(_) => "stop".into(),
        BridgeRequest::StopAsk(_) => "stop_ask".into(),
        BridgeRequest::Status(_) => "status".into(),
    }
}

fn response_variant(resp: &BridgeResponse) -> String {
    match resp {
        BridgeResponse::Pong { .. } => "pong".into(),
        BridgeResponse::AskResult {
            diagnostics: Some(_),
            ..
        } => "ask_result_diagnostics".into(),
        BridgeResponse::AskResult { warning: None, .. } => "ask_result".into(),
        BridgeResponse::AskResult {
            warning: Some(_), ..
        } => "ask_result_warning".into(),
        BridgeResponse::Stopped {
            other_chat: false,
            not_sent: false,
            ..
        } => "stopped".into(),
        BridgeResponse::Stopped {
            other_chat: false,
            not_sent: true,
            ..
        } => "stopped_not_sent".into(),
        BridgeResponse::Stopped {
            other_chat: true, ..
        } => "stopped_other_chat".into(),
        BridgeResponse::StatusResult {
            attachments: false, ..
        } => "status_result".into(),
        BridgeResponse::StatusResult {
            attachments: true, ..
        } => "status_result_attachments".into(),
        BridgeResponse::Error { code, .. } => format!("error_{}", error_code_name(*code)),
    }
}

fn error_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::BadRequest => "bad_request",
        ErrorCode::OversizeFrame => "oversize_frame",
        ErrorCode::ExtensionUnavailable => "extension_unavailable",
        ErrorCode::TabUnavailable => "tab_unavailable",
        ErrorCode::DomFailure => "dom_failure",
        ErrorCode::Timeout => "timeout",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::TabBusy => "tab_busy",
        ErrorCode::Internal => "internal",
    }
}

const REQUESTS: &[&str] = &[
    "ask",
    "ask_conversation",
    "ask_conversation_attachments",
    "ask_conversation_capture",
    "ask_new_chat",
    "ask_new_chat_project",
    "ask_project",
    "ping",
    "status",
    "stop",
    "stop_ask",
];

const RESPONSES: &[&str] = &[
    "ask_result",
    "ask_result_diagnostics",
    "ask_result_warning",
    "error_bad_request",
    "error_cancelled",
    "error_dom_failure",
    "error_extension_unavailable",
    "error_internal",
    "error_oversize_frame",
    "error_rate_limited",
    "error_tab_busy",
    "error_tab_unavailable",
    "error_timeout",
    "pong",
    "status_result",
    "status_result_attachments",
    "stopped",
    "stopped_not_sent",
    "stopped_other_chat",
];

#[test]
fn every_fixture_round_trips_and_is_named_after_its_variant() {
    let all = fixtures();
    let mut requests = BTreeSet::new();
    let mut responses = BTreeSet::new();
    for (name, text, value) in &all {
        let want = value.clone();
        let (variant, back) = if let Some(rest) = name.strip_prefix("request_") {
            let req: BridgeRequest = serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("{name}: not a BridgeRequest: {e}"));
            assert!(requests.insert(rest.to_owned()), "{name}: duplicate");
            (request_variant(&req), serde_json::to_string(&req).unwrap())
        } else if let Some(rest) = name.strip_prefix("response_") {
            let resp: BridgeResponse = serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("{name}: not a BridgeResponse: {e}"));
            assert!(responses.insert(rest.to_owned()), "{name}: duplicate");
            (
                response_variant(&resp),
                serde_json::to_string(&resp).unwrap(),
            )
        } else {
            panic!("{name}: fixture name must start with request_ or response_");
        };
        let stem = name.split_once('_').unwrap().1;
        assert_eq!(
            variant, stem,
            "{name}: file name does not match its variant"
        );
        let back: Value = serde_json::from_str(&back).unwrap();
        assert_eq!(
            back, want,
            "{name}: does not serialize back to the same JSON"
        );
    }
    let expected = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(requests, expected(REQUESTS), "request fixtures");
    assert_eq!(responses, expected(RESPONSES), "response fixtures");
}

#[test]
fn every_error_code_has_a_fixture() {
    // Guard for RESPONSES: listing the codes here (and the exhaustive match
    // in error_code_name) keeps the set complete.
    let all = [
        ErrorCode::BadRequest,
        ErrorCode::OversizeFrame,
        ErrorCode::ExtensionUnavailable,
        ErrorCode::TabUnavailable,
        ErrorCode::DomFailure,
        ErrorCode::Timeout,
        ErrorCode::RateLimited,
        ErrorCode::Cancelled,
        ErrorCode::TabBusy,
        ErrorCode::Internal,
    ];
    for code in all {
        let name = format!("error_{}", error_code_name(code));
        assert!(RESPONSES.contains(&name.as_str()), "no fixture for {name}");
        // The code's wire string is the one the fixture carries.
        assert_eq!(
            serde_json::to_value(code).unwrap(),
            Value::String(error_code_name(code).into())
        );
    }
}

#[test]
fn the_host_answers_the_ping_fixture_with_the_pong_fixture() {
    // At the fixture's time, with this build's version and build id.
    let all = fixtures();
    let text = |name: &str| all.iter().find(|f| f.0 == name).unwrap().1.clone();
    let ping: BridgeRequest = serde_json::from_str(&text("request_ping")).unwrap();
    let mut want: Value = serde_json::from_str(&text("response_pong")).unwrap();
    let ts = want["ts_unix_ms"].as_u64().unwrap();
    let resp = dispatch_basic(ping, ts).unwrap();
    want["host_version"] = HOST_VERSION.into();
    want["build"] = BUILD.into();
    assert_eq!(serde_json::to_value(&resp).unwrap(), want);
}

#[test]
fn build_is_optional_and_a_frame_without_it_passes_unchanged() {
    // An older host (pong) or extension (status_result) sends no `build`: it
    // reads as None, and a host forwarding such a frame adds nothing to it.
    let legacy = fixtures_in("legacy");
    let names: Vec<&str> = legacy.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(names, ["response_pong", "response_status_result"]);
    for (name, text, value) in &legacy {
        assert!(
            value.get("build").is_none(),
            "{name}: a legacy frame has no build"
        );
        let resp: BridgeResponse =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        match &resp {
            BridgeResponse::Pong { build, .. } | BridgeResponse::StatusResult { build, .. } => {
                assert_eq!(*build, None, "{name}")
            }
            other => panic!("{name}: unexpected {other:?}"),
        }
        assert_eq!(&serde_json::to_value(&resp).unwrap(), value, "{name}");
    }
}

/// `pong` and `status_result` as a reader built before TASK-043 declares
/// them (no `build`; serde ignores fields it does not declare). The old
/// `ts_unix_ms` was a u128 that no reader could take at all (fixed here), so
/// the replica has the u64.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum BeforeBuild {
    Pong {
        id: Option<String>,
        host_version: String,
        echo: Option<Value>,
        ts_unix_ms: u64,
    },
    StatusResult {
        id: String,
        extension: bool,
        tab_open: bool,
        logged_in: bool,
        #[serde(default)]
        conversation_url: Option<String>,
    },
}

#[test]
fn a_reader_without_build_still_reads_the_new_frames() {
    let all = fixtures();
    for name in ["response_pong", "response_status_result"] {
        let (_, text, value) = all.iter().find(|f| f.0 == name).unwrap();
        assert!(
            value.get("build").is_some(),
            "{name}: the fixture carries a build"
        );
        serde_json::from_str::<BeforeBuild>(text).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// `ask` as a host built before `project` declares it. Such a host parses the
/// CLI frame into it and forwards that (host/src/main.rs), so `project` is
/// dropped and the chat goes where ChatGPT puts a new one; `warning` on the
/// way back is dropped the same way. Accepted: the host, the extension and
/// the clients ship in one bundle (no `#[serde(flatten)]` pass-through).
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BeforeProject {
    Ask {
        id: String,
        text: String,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<ChatTarget>,
    },
}

#[test]
fn a_host_without_project_reads_the_ask_and_forwards_it_without_the_project() {
    let all = fixtures();
    for name in ["request_ask_project", "request_ask_new_chat_project"] {
        let (_, text, value) = all.iter().find(|f| f.0 == name).unwrap();
        let old: BeforeProject =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut want = value.clone();
        want.as_object_mut().unwrap().remove("project");
        assert_eq!(serde_json::to_value(&old).unwrap(), want, "{name}");
    }
}

/// The same for `attachments`: a host built before it forwards the ask
/// without the files. The gateway therefore sends files only to an
/// extension that reports `attachments: true` in `status_result`, which such
/// a host never passes on (its `status_result` does not declare the field).
#[test]
fn a_host_without_attachments_forwards_the_ask_without_them() {
    let all = fixtures();
    let (_, text, value) = all
        .iter()
        .find(|f| f.0 == "request_ask_conversation_attachments")
        .unwrap();
    let old: BeforeProject = serde_json::from_str(text).unwrap();
    let mut want = value.clone();
    want.as_object_mut().unwrap().remove("attachments");
    assert_eq!(serde_json::to_value(&old).unwrap(), want);
    let (_, text, _) = all
        .iter()
        .find(|f| f.0 == "response_status_result_attachments")
        .unwrap();
    let old = serde_json::from_str::<BeforeBuild>(text).unwrap();
    assert!(!format!("{old:?}").contains("attachments"));
}
