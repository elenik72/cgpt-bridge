//! Transport-level request/response envelopes.
//!
//! These are the messages exchanged between local processes (CLI ↔ host ↔
//! Chrome extension). The assistant-side protocol (`cgpt-agent-response-v1`)
//! is a higher layer and is not modeled here.

use serde::{Deserialize, Serialize};

/// Crate-level version string for the host. Useful in `pong` payloads so
/// clients can tell which build they're talking to.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Inbound message: either Chrome→host (Native Messaging) or CLI→host (UDS).
/// The same envelope works on both because the framing is the same.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeRequest {
    /// Health check. No tab interaction.
    Ping(PingRequest),

    /// "Single ask" request: insert a prompt into the active ChatGPT tab and
    /// return the visible assistant response. Sent by `cgpt ask`; will be
    /// forwarded to the Chrome extension in M5 phase 2.
    Ask(AskRequest),
}

#[derive(Debug, Deserialize)]
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
}

/// Outbound message back to the caller (extension over NM, or CLI over UDS).
///
/// `host_version` is `Cow<'static, str>` rather than `&'static str` so callers
/// that *deserialize* a response (the CLI) can own the string, while the host
/// can still send a `'static` literal at zero cost.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeResponse {
    Pong {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        host_version: std::borrow::Cow<'static, str>,
        echo: Option<serde_json::Value>,
        ts_unix_ms: u128,
    },

    /// Successful ask: the visible assistant response text.
    AskResult { id: String, text: String },

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
    /// Catch-all for anything the host did not anticipate.
    Internal,
}

/// Handle a request that needs no tab interaction (Ping only, for now).
/// `Ask` is intentionally not handled here — that path is routed through the
/// extension and gets a real response from the page.
pub fn dispatch_basic(req: BridgeRequest, ts_unix_ms: u128) -> Option<BridgeResponse> {
    match req {
        BridgeRequest::Ping(p) => Some(BridgeResponse::Pong {
            id: p.id,
            host_version: std::borrow::Cow::Borrowed(HOST_VERSION),
            echo: p.payload,
            ts_unix_ms,
        }),
        BridgeRequest::Ask(_) => None,
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
        BridgeResponse::AskResult { id, .. } => Some(id),
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
}
