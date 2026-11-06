//! The gateway's view of ChatGPT: send a message, get the answer, stop.
//! [`BridgeBackend`] does it through the native host; [`FakeBackend`] stands
//! in for tests.

use std::io::{self, ErrorKind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cgpt_bridge_protocol::{
    connect, default_endpoint, new_request_id, round_trip, AskDiagnostics, AskRequest, Attachment,
    BridgeRequest, BridgeResponse, ChatTarget, ErrorCode, PingRequest, ReadFrameError,
    StatusRequest, StopAskRequest, StopRequest, CLIENT_TIMEOUT_GRACE_MS,
};

#[derive(Debug, Clone, PartialEq)]
pub enum BackendError {
    /// No extension or ChatGPT tab, or the host went away mid-request. The
    /// ask may already have been forwarded.
    Unavailable(String),
    /// The host could not be reached at all: nothing of the request was
    /// sent.
    NotConnected(String),
    /// The host's socket refuses this user (a host run by another user;
    /// Chrome started as administrator on Windows): nothing was sent, and
    /// only a person can fix it.
    NoAccess(String),
    RateLimited,
    /// The gateway's pause after a rate limit: nothing was sent, and nothing
    /// will be before `until_ms` (ms since the Unix epoch).
    CoolingDown {
        until_ms: u128,
    },
    Timeout,
    /// The request's turn did not come within its budget, or too little of
    /// the budget was left once it came or before its first part went out:
    /// nothing was sent.
    QueueTimeout,
    /// ChatGPT's page did not behave (composer or answer not found, ...).
    Dom(String),
    /// The own tab was busy (an earlier answer still generating after Stop,
    /// or another ask running in the page): this ask was refused before
    /// anything was typed. The engine turns it into `Dom` once an earlier
    /// ask of the same request was sent.
    TabBusy(String),
    /// The host or extension predates the gateway: an answer without the
    /// chat's URL (an old extension ignores the target and writes into the
    /// user's active tab), or `stop`/`status` unknown to the host.
    Outdated,
    /// The answer is larger than the bridge's frame limit. Asking again
    /// produces the same answer.
    TooLarge(String),
    /// The turn was stopped on purpose (the Stop button in the ChatGPT page,
    /// or a stop request): the user does not want it resent.
    Cancelled,
    Other(String),
}

/// The builds the bridge's parts reported: the host's (`pong`) and the
/// extension's (`status_result`). `None`: that part sent no build (built
/// before builds were reported).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerBuilds {
    pub host: Option<String>,
    pub extension: Option<String>,
}

/// An answered ask: the text, the chat it came from, the bridge's warning
/// (say, the new chat went to a plain new chat instead of the project), and
/// what the extension noticed about the answer (the stream next to the page,
/// the raw stream when it was asked for).
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub text: String,
    pub conversation_url: Option<String>,
    pub warning: Option<String>,
    pub diagnostics: AskDiagnostics,
}

/// `ask` returns the answer text and the conversation URL it came from.
///
/// Methods take `&self` and the trait is `Sync` on purpose: `stop` has to run
/// while an `ask` of the same backend is still blocked (the client of that
/// ask went away), and asks of different sessions may run at once (the
/// gateway keeps one turn per session, see `lanes`).
pub trait ChatBackend: Send + Sync {
    fn ask(
        &self,
        text: &str,
        target: &ChatTarget,
        timeout_ms: u64,
    ) -> Result<(String, Option<String>), BackendError>;

    /// [`ask`](Self::ask) under a given bridge id (see [`new_ask_id`]). The host
    /// and the extension carry the id, so the gateway log, the host and the
    /// service worker can be matched by it. A backend that has no bridge
    /// ignores the id.
    fn ask_with_id(
        &self,
        _id: &str,
        text: &str,
        target: &ChatTarget,
        timeout_ms: u64,
    ) -> Result<(String, Option<String>), BackendError> {
        self.ask(text, target, timeout_ms)
    }

    /// [`ask_with_id`](Self::ask_with_id) with the ChatGPT project a new chat
    /// goes into (`project`, see `cgpt_bridge_core::project`; the extension
    /// ignores it for a chat that exists), answering with the bridge's
    /// warning as well. A backend that has no bridge ignores the project and
    /// has no warning.
    fn ask_in_project(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        _project: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        let (text, conversation_url) = self.ask_with_id(id, text, target, timeout_ms)?;
        Ok(Answer {
            text,
            conversation_url,
            warning: None,
            diagnostics: AskDiagnostics::default(),
        })
    }

    /// [`ask_in_project`](Self::ask_in_project) with files the page puts into
    /// the composer before the text (`attachments`; the engine sends them
    /// only when [`attachments`](Self::attachments) says the extension takes
    /// them). A backend that has no bridge sends none: it refuses an ask
    /// with files rather than drop them.
    fn ask_with_files(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        attachments: &[Attachment],
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        if !attachments.is_empty() {
            return Err(BackendError::Other(
                "этот бэкенд не передаёт файлы".to_string(),
            ));
        }
        self.ask_in_project(id, text, target, project, timeout_ms)
    }

    /// [`ask_with_files`](Self::ask_with_files), asking the bridge for the
    /// answer's event stream as well (the gateway's `debug_capture_turns`).
    /// A backend without a bridge has none to give.
    fn ask_capturing(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        attachments: &[Attachment],
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        self.ask_with_files(id, text, target, project, attachments, timeout_ms)
    }

    /// Press "Stop generating" in the gateway's ChatGPT tab (an extension
    /// with several own tabs stops every gateway ask).
    fn stop(&self) -> Result<(), BackendError>;

    /// Stop the one ask sent under bridge id `ask_id`, leaving every other
    /// ask alone (several sessions at once). `Outdated` from a bridge (or a
    /// backend) that does not know it; whether the plain [`stop`](Self::stop)
    /// may go instead is `engine::stop_request`'s choice.
    fn stop_ask(&self, _ask_id: &str) -> Result<(), BackendError> {
        Err(BackendError::Outdated)
    }

    /// Whether an extension answered `status`. Only an extension that knows
    /// `status` answers it, so `Ok(true)` proves a current one; an old host
    /// rejects it (`Outdated`) and an old extension never answers (`Timeout`).
    fn status(&self) -> Result<bool, BackendError>;

    /// The builds seen at the last `status` an extension answered; `None`
    /// before one, and from a backend without a bridge.
    fn peer_builds(&self) -> Option<PeerBuilds> {
        None
    }

    /// The own tabs the extension said it keeps (`own_tabs`) at the last
    /// `status` it answered; `None` before one, from an extension or host
    /// older than the field, and from a backend without a bridge.
    fn own_tabs(&self) -> Option<u32> {
        None
    }

    /// Whether the extension said at the last `status` it answered that it
    /// puts an ask's files into the composer (`attachments`); false before
    /// one, from an extension or host older than the field, and from a
    /// backend without a bridge.
    fn attachments(&self) -> bool {
        false
    }
}

impl<T: ChatBackend + ?Sized> ChatBackend for Arc<T> {
    fn ask(
        &self,
        text: &str,
        target: &ChatTarget,
        timeout_ms: u64,
    ) -> Result<(String, Option<String>), BackendError> {
        (**self).ask(text, target, timeout_ms)
    }

    fn ask_with_id(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        timeout_ms: u64,
    ) -> Result<(String, Option<String>), BackendError> {
        (**self).ask_with_id(id, text, target, timeout_ms)
    }

    fn ask_in_project(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        (**self).ask_in_project(id, text, target, project, timeout_ms)
    }

    fn ask_with_files(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        attachments: &[Attachment],
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        (**self).ask_with_files(id, text, target, project, attachments, timeout_ms)
    }

    fn ask_capturing(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        attachments: &[Attachment],
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        (**self).ask_capturing(id, text, target, project, attachments, timeout_ms)
    }

    fn stop(&self) -> Result<(), BackendError> {
        (**self).stop()
    }

    fn stop_ask(&self, ask_id: &str) -> Result<(), BackendError> {
        (**self).stop_ask(ask_id)
    }

    fn status(&self) -> Result<bool, BackendError> {
        (**self).status()
    }

    fn peer_builds(&self) -> Option<PeerBuilds> {
        (**self).peer_builds()
    }

    fn own_tabs(&self) -> Option<u32> {
        (**self).own_tabs()
    }

    fn attachments(&self) -> bool {
        (**self).attachments()
    }
}

/// A new bridge id for one ask.
pub fn new_ask_id() -> String {
    new_request_id("gw_ask")
}

/// The bridge id in the text of a failure that happened after the ask was sent
/// to the bridge (so the host or the extension may have a record under that
/// id). Failures before the send (connect, serialize) are not tagged, and a
/// timeout, rate limit or stop has no text of its own. The one failure not told
/// apart is a write error inside the round trip: it is tagged like a read error.
fn with_bridge_id(e: BackendError, id: &str) -> BackendError {
    let tag = |m: String| format!("{} (id моста: {})", m, id);
    match e {
        BackendError::Unavailable(m) => BackendError::Unavailable(tag(m)),
        BackendError::Dom(m) => BackendError::Dom(tag(m)),
        BackendError::TabBusy(m) => BackendError::TabBusy(tag(m)),
        BackendError::TooLarge(m) => BackendError::TooLarge(tag(m)),
        BackendError::Other(m) => BackendError::Other(tag(m)),
        other => other,
    }
}

const CONNECT_TIMEOUT_MS: u64 = 3_000;
/// For `stop` and `status`: the host waits 10 s for the extension.
const STOP_TIMEOUT_MS: u64 = 15_000;

/// Talks to the native host over its local socket, one connection per request.
pub struct BridgeBackend {
    endpoint: String,
    /// See [`ChatBackend::peer_builds`].
    builds: Mutex<Option<PeerBuilds>>,
    /// See [`ChatBackend::own_tabs`].
    own_tabs: Mutex<Option<u32>>,
    /// See [`ChatBackend::attachments`].
    attachments: Mutex<bool>,
}

impl BridgeBackend {
    pub fn new(endpoint: String) -> BridgeBackend {
        BridgeBackend {
            endpoint,
            builds: Mutex::new(None),
            own_tabs: Mutex::new(None),
            attachments: Mutex::new(false),
        }
    }

    pub fn with_default_endpoint() -> BridgeBackend {
        BridgeBackend::new(default_endpoint())
    }

    /// One `ask` with its files; `capture` asks the extension for the
    /// answer's event stream.
    #[allow(clippy::too_many_arguments)]
    fn ask_bridge(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        attachments: &[Attachment],
        timeout_ms: u64,
        capture: bool,
    ) -> Result<Answer, BackendError> {
        let request = BridgeRequest::Ask(AskRequest {
            id: id.to_string(),
            text: text.to_string(),
            timeout_ms,
            target: Some(target.clone()),
            project: project.map(str::to_string),
            capture_stream: capture,
            attachments: attachments.to_vec(),
        });
        let response = self.exchange(
            &request,
            timeout_ms.saturating_add(CLIENT_TIMEOUT_GRACE_MS),
            Some(id),
        )?;
        match response {
            BridgeResponse::AskResult {
                text,
                conversation_url,
                warning,
                diagnostics,
                ..
            } => Ok(Answer {
                text,
                conversation_url,
                warning,
                diagnostics: diagnostics.unwrap_or_default(),
            }),
            BridgeResponse::Error { code, message, .. } => {
                Err(with_bridge_id(map_error(code, message), id))
            }
            other => Err(with_bridge_id(
                BackendError::Other(format!("неожиданный ответ хоста: {:?}", other)),
                id,
            )),
        }
    }

    /// `id` is the bridge id to name in the text of a failure after the
    /// request was handed to the socket (see [`with_bridge_id`]).
    fn exchange(
        &self,
        request: &BridgeRequest,
        timeout_ms: u64,
        id: Option<&str>,
    ) -> Result<BridgeResponse, BackendError> {
        let stream = connect(&self.endpoint, CONNECT_TIMEOUT_MS).map_err(connect_error)?;
        let body = serde_json::to_vec(request)
            .map_err(|e| BackendError::Other(format!("serialize: {}", e)))?;
        let tag = |e: BackendError| match id {
            Some(id) => with_bridge_id(e, id),
            None => e,
        };
        let bytes = round_trip(stream, &body, Duration::from_millis(timeout_ms)).map_err(|e| {
            tag(match e {
                ReadFrameError::Eof => BackendError::Unavailable(
                    "cgpt-bridge-host закрыл соединение без ответа".to_string(),
                ),
                ReadFrameError::OversizeFrame { len, cap } => BackendError::TooLarge(format!(
                    "ответ хоста слишком большой: {} байт (предел {})",
                    len, cap
                )),
                ReadFrameError::Io(e)
                    if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                {
                    BackendError::Timeout
                }
                ReadFrameError::Io(e) => {
                    BackendError::Other(format!("обмен с cgpt-bridge-host: {}", e))
                }
            })
        })?;
        serde_json::from_slice(&bytes).map_err(|e| {
            tag(BackendError::Other(format!(
                "ответ хоста не разобран: {}",
                e
            )))
        })
    }
}

/// What a failed connect to the host's socket means for the caller.
fn connect_error(e: io::Error) -> BackendError {
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => BackendError::NotConnected(
            "cgpt-bridge-host не запущен или Chrome с расширением закрыт".to_string(),
        ),
        // The host's pipe is owner-only. On Windows the usual cause is a
        // Chrome started as administrator, which runs the host under another
        // user. Nothing was sent, but a resend cannot fix it.
        ErrorKind::PermissionDenied => BackendError::NoAccess(if cfg!(windows) {
            format!(
                "нет доступа к cgpt-bridge-host (Chrome запущен от администратора?): {}",
                e
            )
        } else {
            format!(
                "нет прав на сокет cgpt-bridge-host (хост запущен другим пользователем?): {}",
                e
            )
        }),
        _ => BackendError::Other(format!("подключение к cgpt-bridge-host: {}", e)),
    }
}

fn map_error(code: ErrorCode, message: String) -> BackendError {
    match code {
        // For an ask, `timeout` is a real timeout (HTTP 504). The host also
        // answers `timeout` to a `status` sent to a silent extension; that
        // case is "no extension" and is handled where `status` is used.
        ErrorCode::ExtensionUnavailable | ErrorCode::TabUnavailable => {
            BackendError::Unavailable(message)
        }
        ErrorCode::RateLimited => BackendError::RateLimited,
        ErrorCode::Timeout => BackendError::Timeout,
        ErrorCode::DomFailure => BackendError::Dom(message),
        // An extension older than `tab_busy` (or one behind such a host)
        // sends it as `dom_failure`, which is not resent.
        ErrorCode::TabBusy => BackendError::TabBusy(message),
        // serde's words for a request type the host does not know.
        ErrorCode::BadRequest if message.contains("unknown variant") => BackendError::Outdated,
        // The gateway keeps its own request within the frame limit, so this
        // is the answer.
        ErrorCode::OversizeFrame => BackendError::TooLarge(message),
        ErrorCode::Cancelled => BackendError::Cancelled,
        // Fallback for an extension built before `cancelled` (or one talking
        // to such a host): it reports a stop as `internal` with these
        // messages. Remove once those extensions are gone.
        ErrorCode::Internal if is_user_stop(&message) => BackendError::Cancelled,
        ErrorCode::BadRequest | ErrorCode::Internal => BackendError::Other(message),
    }
}

/// The fallback above: the extension's exact words for a stopped turn,
/// `CancelledError`'s default
/// and the page-stop text in content.ts. Pinned on both sides by
/// protocol/tests/fixtures/stop_messages.txt (a test here, one in the
/// extension): a reworded message must fail a test, not turn a Stop back into
/// a retried 500.
const USER_STOP_MESSAGES: [&str; 2] = [
    "cancelled by stop",
    "The answer was stopped in the ChatGPT page before it ended.",
];

fn is_user_stop(message: &str) -> bool {
    USER_STOP_MESSAGES.contains(&message)
}

impl ChatBackend for BridgeBackend {
    fn ask(
        &self,
        text: &str,
        target: &ChatTarget,
        timeout_ms: u64,
    ) -> Result<(String, Option<String>), BackendError> {
        self.ask_with_id(&new_ask_id(), text, target, timeout_ms)
    }

    fn ask_with_id(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        timeout_ms: u64,
    ) -> Result<(String, Option<String>), BackendError> {
        self.ask_in_project(id, text, target, None, timeout_ms)
            .map(|a| (a.text, a.conversation_url))
    }

    fn ask_in_project(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        self.ask_bridge(id, text, target, project, &[], timeout_ms, false)
    }

    fn ask_with_files(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        attachments: &[Attachment],
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        self.ask_bridge(id, text, target, project, attachments, timeout_ms, false)
    }

    fn ask_capturing(
        &self,
        id: &str,
        text: &str,
        target: &ChatTarget,
        project: Option<&str>,
        attachments: &[Attachment],
        timeout_ms: u64,
    ) -> Result<Answer, BackendError> {
        self.ask_bridge(id, text, target, project, attachments, timeout_ms, true)
    }

    fn stop(&self) -> Result<(), BackendError> {
        let request = BridgeRequest::Stop(StopRequest {
            id: new_request_id("gw_stop"),
        });
        match self.exchange(&request, STOP_TIMEOUT_MS, None)? {
            BridgeResponse::Stopped { .. } => Ok(()),
            BridgeResponse::Error { code, message, .. } => Err(map_error(code, message)),
            other => Err(BackendError::Other(format!(
                "неожиданный ответ хоста: {:?}",
                other
            ))),
        }
    }

    fn stop_ask(&self, ask_id: &str) -> Result<(), BackendError> {
        let request = BridgeRequest::StopAsk(StopAskRequest {
            id: new_request_id("gw_stop"),
            ask_id: ask_id.to_string(),
        });
        // A host older than `stop_ask` answers `Outdated` (unknown variant).
        match self.exchange(&request, STOP_TIMEOUT_MS, None)? {
            BridgeResponse::Stopped { .. } => Ok(()),
            BridgeResponse::Error { code, message, .. } => Err(map_error(code, message)),
            other => Err(BackendError::Other(format!(
                "неожиданный ответ хоста: {:?}",
                other
            ))),
        }
    }

    fn status(&self) -> Result<bool, BackendError> {
        let request = BridgeRequest::Status(StatusRequest {
            id: new_request_id("gw_status"),
        });
        match self.exchange(&request, STOP_TIMEOUT_MS, None)? {
            BridgeResponse::StatusResult {
                extension,
                build,
                own_tabs,
                attachments,
                ..
            } => {
                if extension {
                    *self.own_tabs.lock().unwrap() = own_tabs;
                    *self.attachments.lock().unwrap() = attachments;
                    let host = self.host_build();
                    let mut builds = self.builds.lock().unwrap();
                    // A failed ping says nothing about the host's build: keep
                    // the one known, and with none known record nothing yet.
                    let host = match (host, builds.as_ref()) {
                        (Some(host), _) => Some(host),
                        (None, Some(known)) => Some(known.host.clone()),
                        (None, None) => None,
                    };
                    if let Some(host) = host {
                        *builds = Some(PeerBuilds {
                            host,
                            extension: build,
                        });
                    }
                }
                Ok(extension)
            }
            BridgeResponse::Error { code, message, .. } => Err(map_error(code, message)),
            other => Err(BackendError::Other(format!(
                "неожиданный ответ хоста: {:?}",
                other
            ))),
        }
    }

    fn peer_builds(&self) -> Option<PeerBuilds> {
        self.builds.lock().unwrap().clone()
    }

    fn own_tabs(&self) -> Option<u32> {
        *self.own_tabs.lock().unwrap()
    }

    fn attachments(&self) -> bool {
        *self.attachments.lock().unwrap()
    }
}

impl BridgeBackend {
    /// The host's build from a `ping` (the host answers it itself):
    /// `Some(None)` when its `pong` has none, `None` when the ping fails.
    fn host_build(&self) -> Option<Option<String>> {
        let ping = BridgeRequest::Ping(PingRequest {
            id: Some(new_request_id("gw_ping")),
            payload: None,
        });
        match self.exchange(&ping, CONNECT_TIMEOUT_MS, None) {
            Ok(BridgeResponse::Pong { build, .. }) => Some(build),
            _ => None,
        }
    }
}

#[cfg(test)]
pub use fake::{FakeBackend, IN_TARGET_CHAT};

#[cfg(test)]
mod fake {
    use super::*;
    use cgpt_bridge_protocol::StreamCheck;
    use std::collections::{HashSet, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    type AskResult = Result<(String, Option<String>), BackendError>;
    type Responder = dyn Fn(&str, &ChatTarget) -> AskResult + Send + Sync;

    /// A scripted chat URL meaning "the chat the ask was sent to", for asks
    /// whose order is not known in advance.
    pub const IN_TARGET_CHAT: &str = "fake:in-target-chat";

    /// Scripted backend: answers from `replies` (an error when empty) and
    /// `statuses` (a current extension, `Ok(true)`, when empty), records what
    /// it was asked, and counts `stop` calls, `status` probes and the largest
    /// number of asks running at once. A running `ask` ends early when `stop` is called.
    ///
    /// For tests that must not depend on timing there is a gate: while it is
    /// closed (`close_gate`), every `ask` blocks after it has recorded itself,
    /// until the test opens it (`open_gate`) or `stop` is called.
    #[derive(Default)]
    pub struct FakeBackend {
        pub replies: Mutex<VecDeque<AskResult>>,
        pub sent: Mutex<Vec<(String, ChatTarget)>>,
        /// The bridge id of every `ask_with_id`, in order.
        pub ids: Mutex<Vec<String>>,
        /// `stop` and `stop_ask` calls together.
        pub stops: AtomicUsize,
        /// The bridge id of every `stop_ask`, in order.
        pub stop_ask_ids: Mutex<Vec<String>>,
        /// What `own_tabs` reports.
        pub own_tabs: Mutex<Option<u32>>,
        /// When set, answers every ask instead of `replies` (for asks of
        /// several sessions at once, whose order is not known). Called
        /// outside any lock of the fake, so it may block one ask.
        responder: Mutex<Option<Arc<Responder>>>,
        /// Asks a `stop_ask` has named: each ends early, the others go on.
        stopped_ids: Mutex<HashSet<String>>,
        /// What every `stop_ask` fails with, when set.
        stop_ask_error: Mutex<Option<BackendError>>,
        /// How long every `ask` takes.
        pub delay: Mutex<Duration>,
        /// How far every `ask` moves the turn's clock on, without a real
        /// wait (`engine::test_clock`).
        pub turn_time: Mutex<Duration>,
        /// How long every `stop` blocks.
        pub stop_delay: Mutex<Duration>,
        /// Ordered `ask:<text>` / `stop_begin` / `stop_end` markers.
        pub log: Mutex<Vec<String>>,
        pub max_concurrent: AtomicUsize,
        /// Answers to `status`, in order (`Ok(true)` when empty).
        pub statuses: Mutex<VecDeque<Result<bool, BackendError>>>,
        /// What `peer_builds` reports.
        pub builds: Mutex<Option<PeerBuilds>>,
        /// The project of every ask, in order (`None` for one sent without).
        pub projects: Mutex<Vec<Option<String>>>,
        /// The warning every answered `ask_in_project` carries.
        pub warning: Mutex<Option<String>>,
        /// The stream check every answered ask carries.
        pub stream_check: Mutex<Option<StreamCheck>>,
        /// The raw stream every `ask_capturing` returns.
        pub capture: Mutex<Option<String>>,
        /// The bridge id of every `ask_capturing`, in order.
        pub capturing_ids: Mutex<Vec<String>>,
        /// The files of every `ask_with_files`, in order (empty for an ask
        /// sent without).
        pub files: Mutex<Vec<Vec<Attachment>>>,
        /// What `attachments` reports.
        pub takes_files: AtomicBool,
        status_probes: AtomicUsize,
        in_flight: AtomicUsize,
        stopped: AtomicBool,
        gate: Mutex<AskGate>,
        gate_changed: Condvar,
    }

    #[derive(Default)]
    struct AskGate {
        closed: bool,
        /// Asks that have recorded themselves so far.
        started: usize,
    }

    /// How long a closed gate may hold an `ask` before the test is failed
    /// instead of hanging.
    const GATE_LIMIT: Duration = Duration::from_secs(60);

    impl FakeBackend {
        pub fn new() -> Arc<FakeBackend> {
            Arc::new(FakeBackend::default())
        }

        pub fn push_reply(&self, reply: AskResult) {
            self.replies.lock().unwrap().push_back(reply);
        }

        pub fn push_status(&self, status: Result<bool, BackendError>) {
            self.statuses.lock().unwrap().push_back(status);
        }

        pub fn status_probes(&self) -> usize {
            self.status_probes.load(Ordering::SeqCst)
        }

        pub fn set_delay(&self, delay: Duration) {
            *self.delay.lock().unwrap() = delay;
        }

        pub fn set_turn_time(&self, time: Duration) {
            *self.turn_time.lock().unwrap() = time;
        }

        pub fn set_stop_delay(&self, delay: Duration) {
            *self.stop_delay.lock().unwrap() = delay;
        }

        pub fn set_own_tabs(&self, own_tabs: Option<u32>) {
            *self.own_tabs.lock().unwrap() = own_tabs;
        }

        pub fn set_attachments(&self, takes_files: bool) {
            self.takes_files.store(takes_files, Ordering::SeqCst);
        }

        pub fn sent_files(&self) -> Vec<Vec<Attachment>> {
            self.files.lock().unwrap().clone()
        }

        pub fn set_responder(
            &self,
            responder: impl Fn(&str, &ChatTarget) -> AskResult + Send + Sync + 'static,
        ) {
            *self.responder.lock().unwrap() = Some(Arc::new(responder));
        }

        pub fn stop_ask_ids(&self) -> Vec<String> {
            self.stop_ask_ids.lock().unwrap().clone()
        }

        /// The plain `stop` calls (of the own tab).
        pub fn plain_stops(&self) -> usize {
            self.stops.load(Ordering::SeqCst) - self.stop_ask_ids.lock().unwrap().len()
        }

        /// Make every `stop_ask` fail with `e` (recorded, nothing stopped).
        pub fn set_stop_ask_error(&self, e: Option<BackendError>) {
            *self.stop_ask_error.lock().unwrap() = e;
        }

        /// Whether the ask `id` is to end now: a `stop`, or a `stop_ask`
        /// naming it.
        fn is_stopped(&self, id: &str) -> bool {
            self.stopped.load(Ordering::SeqCst) || self.stopped_ids.lock().unwrap().contains(id)
        }

        /// Make every following `ask` wait for `open_gate` (or `stop`).
        pub fn close_gate(&self) {
            self.gate.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        }

        pub fn open_gate(&self) {
            self.gate.lock().unwrap_or_else(|e| e.into_inner()).closed = false;
            self.gate_changed.notify_all();
        }

        /// Block until `n` asks have recorded themselves (they may still be
        /// held by the gate); false after `GATE_LIMIT`.
        pub fn wait_for_asks(&self, n: usize) -> bool {
            let deadline = std::time::Instant::now() + GATE_LIMIT;
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            while gate.started < n {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return false;
                }
                gate = self
                    .gate_changed
                    .wait_timeout(gate, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            true
        }

        pub fn sent_texts(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(t, _)| t.clone())
                .collect()
        }

        pub fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl ChatBackend for FakeBackend {
        fn ask(&self, text: &str, target: &ChatTarget, timeout_ms: u64) -> AskResult {
            self.ask_with_id(&new_ask_id(), text, target, timeout_ms)
        }

        fn ask_with_id(
            &self,
            id: &str,
            text: &str,
            target: &ChatTarget,
            _timeout_ms: u64,
        ) -> AskResult {
            self.ids.lock().unwrap().push(id.to_string());
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent.fetch_max(now, Ordering::SeqCst);
            self.stopped.store(false, Ordering::SeqCst);
            self.log.lock().unwrap().push(format!("ask:{}", text));
            self.sent
                .lock()
                .unwrap()
                .push((text.to_string(), target.clone()));
            {
                let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
                gate.started += 1;
                self.gate_changed.notify_all();
                let deadline = std::time::Instant::now() + GATE_LIMIT;
                while gate.closed && !self.is_stopped(id) {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    assert!(!left.is_zero(), "the test never opened the ask gate");
                    gate = self
                        .gate_changed
                        .wait_timeout(gate, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
            let delay = *self.delay.lock().unwrap();
            let started = std::time::Instant::now();
            while started.elapsed() < delay && !self.is_stopped(id) {
                std::thread::sleep(Duration::from_millis(5));
            }
            crate::engine::test_clock::advance(*self.turn_time.lock().unwrap());
            let responder = self.responder.lock().unwrap().clone();
            let scripted =
                match responder {
                    Some(respond) => respond(text, target),
                    None => self.replies.lock().unwrap().pop_front().unwrap_or_else(|| {
                        Err(BackendError::Other("no scripted reply".to_string()))
                    }),
                };
            let reply = scripted.map(|(text, url)| match (url, target) {
                (Some(u), ChatTarget::Conversation { url }) if u == IN_TARGET_CHAT => {
                    (text, Some(url.clone()))
                }
                (url, _) => (text, url),
            });
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            reply
        }

        fn ask_in_project(
            &self,
            id: &str,
            text: &str,
            target: &ChatTarget,
            project: Option<&str>,
            timeout_ms: u64,
        ) -> Result<Answer, BackendError> {
            self.projects
                .lock()
                .unwrap()
                .push(project.map(str::to_string));
            let (text, conversation_url) = self.ask_with_id(id, text, target, timeout_ms)?;
            Ok(Answer {
                text,
                conversation_url,
                warning: self.warning.lock().unwrap().clone(),
                diagnostics: AskDiagnostics {
                    stream_check: self.stream_check.lock().unwrap().clone(),
                    ..AskDiagnostics::default()
                },
            })
        }

        fn ask_with_files(
            &self,
            id: &str,
            text: &str,
            target: &ChatTarget,
            project: Option<&str>,
            attachments: &[Attachment],
            timeout_ms: u64,
        ) -> Result<Answer, BackendError> {
            self.files.lock().unwrap().push(attachments.to_vec());
            self.ask_in_project(id, text, target, project, timeout_ms)
        }

        fn ask_capturing(
            &self,
            id: &str,
            text: &str,
            target: &ChatTarget,
            project: Option<&str>,
            attachments: &[Attachment],
            timeout_ms: u64,
        ) -> Result<Answer, BackendError> {
            self.capturing_ids.lock().unwrap().push(id.to_string());
            let mut answer =
                self.ask_with_files(id, text, target, project, attachments, timeout_ms)?;
            answer.diagnostics.stream_capture = self.capture.lock().unwrap().clone();
            Ok(answer)
        }

        fn attachments(&self) -> bool {
            self.takes_files.load(Ordering::SeqCst)
        }

        fn stop(&self) -> Result<(), BackendError> {
            self.log.lock().unwrap().push("stop_begin".to_string());
            self.stops.fetch_add(1, Ordering::SeqCst);
            {
                // Under the gate's lock, so that an ask about to wait sees it.
                let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
                self.stopped.store(true, Ordering::SeqCst);
                self.gate_changed.notify_all();
            }
            std::thread::sleep(*self.stop_delay.lock().unwrap());
            self.log.lock().unwrap().push("stop_end".to_string());
            Ok(())
        }

        /// Like `stop`, but ends only the ask `ask_id` names.
        fn stop_ask(&self, ask_id: &str) -> Result<(), BackendError> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.stop_ask_ids.lock().unwrap().push(ask_id.to_string());
            if let Some(e) = self.stop_ask_error.lock().unwrap().clone() {
                return Err(e);
            }
            self.log.lock().unwrap().push("stop_begin".to_string());
            {
                let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
                self.stopped_ids.lock().unwrap().insert(ask_id.to_string());
                self.gate_changed.notify_all();
            }
            std::thread::sleep(*self.stop_delay.lock().unwrap());
            self.log.lock().unwrap().push("stop_end".to_string());
            Ok(())
        }

        fn own_tabs(&self) -> Option<u32> {
            *self.own_tabs.lock().unwrap()
        }

        fn status(&self) -> Result<bool, BackendError> {
            self.status_probes.fetch_add(1, Ordering::SeqCst);
            self.statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(true))
        }

        fn peer_builds(&self) -> Option<PeerBuilds> {
            self.builds.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cgpt_bridge_protocol::{accept, listen, read_frame, release, write_frame};
    use std::io::{BufReader, BufWriter, Write};

    fn endpoint(tag: &str) -> String {
        #[cfg(unix)]
        {
            format!("/tmp/cgw-be-{}-{}.sock", std::process::id(), tag)
        }
        #[cfg(windows)]
        {
            format!("cgw-be-{}-{}", std::process::id(), tag)
        }
    }

    /// One-shot host: reads a request frame, answers with `reply`, returns
    /// the request JSON.
    fn fake_host(
        tag: &str,
        reply: serde_json::Value,
    ) -> (String, std::thread::JoinHandle<serde_json::Value>) {
        let ep = endpoint(tag);
        let listener = listen(&ep).unwrap();
        let ep2 = ep.clone();
        let handle = std::thread::spawn(move || {
            let stream = accept(&listener).unwrap();
            let request = read_frame(&mut BufReader::new(&stream)).unwrap();
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, reply.to_string().as_bytes()).unwrap();
            writer.flush().unwrap();
            drop(writer);
            let value = serde_json::from_slice(&request).unwrap();
            release(&ep2);
            value
        });
        (ep, handle)
    }

    /// Host for several connections in a row: answers the n-th request with
    /// `replies[n]`, returns the requests.
    fn fake_host_seq(
        tag: &str,
        replies: Vec<serde_json::Value>,
    ) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
        let ep = endpoint(tag);
        let listener = listen(&ep).unwrap();
        let ep2 = ep.clone();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in replies {
                let stream = accept(&listener).unwrap();
                let request = read_frame(&mut BufReader::new(&stream)).unwrap();
                let mut writer = BufWriter::new(&stream);
                write_frame(&mut writer, reply.to_string().as_bytes()).unwrap();
                writer.flush().unwrap();
                requests.push(serde_json::from_slice(&request).unwrap());
            }
            release(&ep2);
            requests
        });
        (ep, handle)
    }

    fn fixture(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn bridge_backend_status_records_the_builds_of_the_extension_and_the_host() {
        // New parts: the extension's build in `status_result`, the host's in
        // `pong`. Parts built before TASK-043 report none (legacy fixtures).
        let new = (
            include_str!("../../protocol/tests/fixtures/response_status_result.json"),
            include_str!("../../protocol/tests/fixtures/response_pong.json"),
            Some("1a2b3c4d5e6f".to_string()),
        );
        let old = (
            include_str!("../../protocol/tests/fixtures/legacy/response_status_result.json"),
            include_str!("../../protocol/tests/fixtures/legacy/response_pong.json"),
            None,
        );
        for (tag, (status, pong, build)) in [("builds-new", new), ("builds-old", old)] {
            let (ep, host) = fake_host_seq(tag, vec![fixture(status), fixture(pong)]);
            let backend = BridgeBackend::new(ep);
            assert_eq!(
                backend.peer_builds(),
                None,
                "{tag}: nothing seen before a status"
            );
            assert_eq!(backend.status(), Ok(true), "{tag}");
            let requests = host.join().unwrap();
            assert_eq!(requests[0]["type"], "status", "{tag}");
            assert_eq!(requests[1]["type"], "ping", "{tag}");
            assert_eq!(
                backend.peer_builds(),
                Some(PeerBuilds {
                    host: build.clone(),
                    extension: build,
                }),
                "{tag}"
            );
        }
    }

    #[test]
    fn a_failed_ping_keeps_the_last_known_host_build() {
        let status = |build: &str| {
            serde_json::json!({"type":"status_result","id":"x","extension":true,
                "tab_open":true,"logged_in":true,"build":build})
        };
        let pong = serde_json::json!({"type":"pong","id":"y","host_version":"0.0.1",
            "build":"h1","echo":null,"ts_unix_ms":1});
        let garbled = serde_json::json!("not a frame");
        // The first ping fails: no host build known yet, nothing recorded
        // (rather than a `host (none)` that is only a failed ping).
        let (ep, host) = fake_host_seq(
            "builds-ping-fail",
            vec![
                status("x1"),
                garbled.clone(),
                status("x1"),
                pong,
                status("x2"),
                garbled,
            ],
        );
        let backend = BridgeBackend::new(ep);
        assert_eq!(backend.status(), Ok(true));
        assert_eq!(backend.peer_builds(), None);
        assert_eq!(backend.status(), Ok(true));
        let known = PeerBuilds {
            host: Some("h1".into()),
            extension: Some("x1".into()),
        };
        assert_eq!(backend.peer_builds(), Some(known));
        // A later ping fails: the host keeps its known build, the extension's
        // is the new one.
        assert_eq!(backend.status(), Ok(true));
        assert_eq!(
            backend.peer_builds(),
            Some(PeerBuilds {
                host: Some("h1".into()),
                extension: Some("x2".into()),
            })
        );
        host.join().unwrap();
    }

    #[test]
    fn bridge_backend_without_an_extension_records_no_builds() {
        let (ep, host) = fake_host(
            "builds-noext",
            serde_json::json!({"type":"status_result","id":"x","extension":false,
                "tab_open":false,"logged_in":false}),
        );
        let backend = BridgeBackend::new(ep);
        assert_eq!(backend.status(), Ok(false));
        host.join().unwrap();
        assert_eq!(backend.peer_builds(), None);
    }

    #[test]
    fn bridge_backend_ask_sends_target_and_returns_text_and_url() {
        let (ep, host) = fake_host(
            "ask",
            serde_json::json!({"type":"ask_result","id":"x","text":"привет 🎉","conversation_url":"https://chatgpt.com/c/1"}),
        );
        let backend = BridgeBackend::new(ep);
        let (text, url) = backend.ask("hi", &ChatTarget::NewChat, 5_000).unwrap();
        assert_eq!(text, "привет 🎉");
        assert_eq!(url.as_deref(), Some("https://chatgpt.com/c/1"));
        let request = host.join().unwrap();
        assert_eq!(request["type"], "ask");
        assert_eq!(request["text"], "hi");
        assert_eq!(request["timeout_ms"], 5_000);
        assert_eq!(request["target"], serde_json::json!({"kind":"new_chat"}));
        assert!(request["id"].as_str().unwrap().starts_with("gw_ask_"));
    }

    #[test]
    fn bridge_backend_sends_the_project_and_returns_the_warning() {
        let (ep, host) = fake_host(
            "project",
            serde_json::json!({"type":"ask_result","id":"gw_ask_p","text":"ok","conversation_url":"https://chatgpt.com/c/1","warning":"not in the project"}),
        );
        let answer = BridgeBackend::new(ep)
            .ask_in_project(
                "gw_ask_p",
                "hi",
                &ChatTarget::NewChat,
                Some("cgpt-bridge"),
                5_000,
            )
            .unwrap();
        assert_eq!(
            answer,
            Answer {
                text: "ok".into(),
                conversation_url: Some("https://chatgpt.com/c/1".into()),
                warning: Some("not in the project".into()),
                diagnostics: AskDiagnostics::default(),
            }
        );
        let request = host.join().unwrap();
        assert_eq!(request["project"], "cgpt-bridge");
        // No capture unless asked for.
        assert!(request.get("capture_stream").is_none());
        // Without a project the field is left out.
        let (ep, host) = fake_host(
            "no-project",
            serde_json::json!({"type":"ask_result","id":"gw_ask_n","text":"ok","conversation_url":"https://chatgpt.com/c/1"}),
        );
        let answer = BridgeBackend::new(ep)
            .ask_in_project("gw_ask_n", "hi", &ChatTarget::NewChat, None, 5_000)
            .unwrap();
        assert_eq!(answer.warning, None);
        assert!(host.join().unwrap().get("project").is_none());
    }

    #[test]
    fn a_capturing_ask_asks_for_the_stream_and_returns_the_diagnostics() {
        let (ep, host) = fake_host(
            "capture",
            serde_json::json!({"type":"ask_result","id":"gw_ask_c","text":"ok",
                "conversation_url":"https://chatgpt.com/c/1",
                "diagnostics":{"stream_check":{"stream_chars":4,"page_chars":8,"mismatch":true},
                    "stream_capture":"data: {}\n\n"}}),
        );
        let answer = BridgeBackend::new(ep)
            .ask_capturing("gw_ask_c", "hi", &ChatTarget::NewChat, None, &[], 5_000)
            .unwrap();
        assert_eq!(host.join().unwrap()["capture_stream"], true);
        assert_eq!(
            answer.diagnostics,
            AskDiagnostics {
                stream_check: Some(cgpt_bridge_protocol::StreamCheck {
                    stream_chars: 4,
                    page_chars: 8,
                    mismatch: true
                }),
                stream_capture: Some("data: {}\n\n".into()),
                stream_capture_dropped: false,
            }
        );
    }

    #[test]
    fn bridge_backend_sends_the_files_and_leaves_the_field_out_without_them() {
        let png = Attachment {
            name: "image-1.png".into(),
            mime: "image/png".into(),
            data_base64: "iVBORw0KGgo=".into(),
        };
        let (ep, host) = fake_host(
            "files",
            serde_json::json!({"type":"ask_result","id":"gw_ask_f","text":"ok","conversation_url":"https://chatgpt.com/c/1"}),
        );
        BridgeBackend::new(ep)
            .ask_with_files(
                "gw_ask_f",
                "hi",
                &ChatTarget::NewChat,
                None,
                std::slice::from_ref(&png),
                5_000,
            )
            .unwrap();
        assert_eq!(
            host.join().unwrap()["attachments"],
            serde_json::json!([{"name":"image-1.png","mime":"image/png","data_base64":"iVBORw0KGgo="}])
        );
        let (ep, host) = fake_host(
            "no-files",
            serde_json::json!({"type":"ask_result","id":"gw_ask_g","text":"ok","conversation_url":"https://chatgpt.com/c/1"}),
        );
        BridgeBackend::new(ep)
            .ask_in_project("gw_ask_g", "hi", &ChatTarget::NewChat, None, 5_000)
            .unwrap();
        assert!(host.join().unwrap().get("attachments").is_none());
    }

    #[test]
    fn bridge_backend_records_whether_the_extension_takes_files() {
        let status = |attachments: Option<bool>, extension: bool| {
            let mut v = serde_json::json!({"type":"status_result","id":"x",
                "extension":extension,"tab_open":true,"logged_in":true});
            if let Some(a) = attachments {
                v["attachments"] = serde_json::json!(a);
            }
            v
        };
        let pong = serde_json::json!({"type":"pong","id":"y","host_version":"0.0.1",
            "build":"h1","echo":null,"ts_unix_ms":1});
        let (ep, host) = fake_host_seq(
            "takes-files",
            vec![
                status(Some(true), true),
                pong.clone(),
                // No extension: says nothing about it.
                status(None, false),
                // An extension without the field (older): no files.
                status(None, true),
                pong,
            ],
        );
        let backend = BridgeBackend::new(ep);
        assert!(!backend.attachments(), "nothing before a status");
        assert_eq!(backend.status(), Ok(true));
        assert!(backend.attachments());
        assert_eq!(backend.status(), Ok(false));
        assert!(backend.attachments());
        assert_eq!(backend.status(), Ok(true));
        assert!(!backend.attachments());
        host.join().unwrap();
    }

    #[test]
    fn bridge_backend_sends_the_given_id_to_the_host() {
        let (ep, host) = fake_host(
            "given-id",
            serde_json::json!({"type":"ask_result","id":"gw_ask_given","text":"ok","conversation_url":"https://chatgpt.com/c/1"}),
        );
        BridgeBackend::new(ep)
            .ask_with_id("gw_ask_given", "hi", &ChatTarget::NewChat, 5_000)
            .unwrap();
        assert_eq!(host.join().unwrap()["id"], "gw_ask_given");
    }

    #[test]
    fn bridge_backend_failure_text_names_the_bridge_id() {
        // The host went away without an answer: the ask may be in the chat.
        let ep = endpoint("eof-id");
        let listener = listen(&ep).unwrap();
        let ep2 = ep.clone();
        let handle = std::thread::spawn(move || {
            let stream = accept(&listener).unwrap();
            let _ = read_frame(&mut BufReader::new(&stream)).unwrap();
            drop(stream);
            release(&ep2);
        });
        let err = BridgeBackend::new(ep)
            .ask_with_id("gw_ask_eof", "hi", &ChatTarget::NewChat, 5_000)
            .unwrap_err();
        handle.join().unwrap();
        match err {
            BackendError::Unavailable(m) => assert!(m.contains("gw_ask_eof"), "{m}"),
            other => panic!("{other:?}"),
        }
        // Nothing reached the bridge when it cannot be connected to: no id.
        let err = BridgeBackend::new(endpoint("nobody-home"))
            .ask_with_id("gw_ask_none", "hi", &ChatTarget::NewChat, 5_000)
            .unwrap_err();
        assert!(
            matches!(err, BackendError::NotConnected(ref m) if !m.contains("gw_ask_none")),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_before_the_ask_was_sent_does_not_name_the_bridge_id() {
        // A socket path over the OS limit: the connect fails with an error
        // that is neither "not found" nor "refused", before anything is sent.
        let ep = format!("/tmp/cgw-be-{}", "x".repeat(300));
        let err = BridgeBackend::new(ep)
            .ask_with_id("gw_ask_pre", "hi", &ChatTarget::NewChat, 1_000)
            .unwrap_err();
        match err {
            BackendError::Other(m) => assert!(!m.contains("gw_ask_pre"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_garbled_answer_names_the_bridge_id() {
        // The ask went out and something came back that is not a frame.
        let ep = endpoint("garbled-id");
        let listener = listen(&ep).unwrap();
        let ep2 = ep.clone();
        let handle = std::thread::spawn(move || {
            let stream = accept(&listener).unwrap();
            let _ = read_frame(&mut BufReader::new(&stream)).unwrap();
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, b"not json").unwrap();
            writer.flush().unwrap();
            drop(writer);
            release(&ep2);
        });
        let err = BridgeBackend::new(ep)
            .ask_with_id("gw_ask_bad", "hi", &ChatTarget::NewChat, 5_000)
            .unwrap_err();
        handle.join().unwrap();
        match err {
            BackendError::Other(m) => assert!(m.contains("gw_ask_bad"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bridge_backend_maps_host_error_codes() {
        for (code, expected) in [
            (
                "extension_unavailable",
                BackendError::Unavailable("m".into()),
            ),
            ("tab_unavailable", BackendError::Unavailable("m".into())),
            ("rate_limited", BackendError::RateLimited),
            ("timeout", BackendError::Timeout),
            ("dom_failure", BackendError::Dom("m".into())),
            // Whatever the message: the code says the user stopped the turn.
            ("cancelled", BackendError::Cancelled),
            ("tab_busy", BackendError::TabBusy("m".into())),
            ("internal", BackendError::Other("m".into())),
        ] {
            let (ep, host) = fake_host(
                &format!("err-{}", code),
                serde_json::json!({"type":"error","id":"x","code":code,"message":"m"}),
            );
            let got = BridgeBackend::new(ep)
                .ask_with_id("gw_ask_t", "hi", &ChatTarget::NewChat, 5_000)
                .unwrap_err();
            // A failure with a text of its own names the bridge id in it.
            let expected = match expected {
                BackendError::Unavailable(m) => {
                    BackendError::Unavailable(format!("{m} (id моста: gw_ask_t)"))
                }
                BackendError::Dom(m) => BackendError::Dom(format!("{m} (id моста: gw_ask_t)")),
                BackendError::TabBusy(m) => {
                    BackendError::TabBusy(format!("{m} (id моста: gw_ask_t)"))
                }
                BackendError::Other(m) => BackendError::Other(format!("{m} (id моста: gw_ask_t)")),
                other => other,
            };
            assert_eq!(got, expected, "code {}", code);
            host.join().unwrap();
        }
    }

    #[test]
    fn bridge_backend_tells_an_oversize_answer_and_a_user_stop_from_other_failures() {
        // The extension's own words for a turn stopped in the ChatGPT page
        // (content.ts) or by a stop request (CancelledError default). The
        // kind is read from the host's message, before the bridge id is added
        // to a failure's text.
        let oversize = "ответ 2000000 байт больше лимита моста (1048576)";
        for (code, message, expected) in [
            (
                "oversize_frame",
                oversize,
                BackendError::TooLarge(format!("{} (id моста: gw_ask_kind)", oversize)),
            ),
            (
                "internal",
                "The answer was stopped in the ChatGPT page before it ended.",
                BackendError::Cancelled,
            ),
            ("internal", "cancelled by stop", BackendError::Cancelled),
            (
                "internal",
                "boom",
                BackendError::Other("boom (id моста: gw_ask_kind)".into()),
            ),
        ] {
            let (ep, host) = fake_host(
                &format!("err-{}-{}", code, message.len()),
                serde_json::json!({"type":"error","id":"x","code":code,"message":message}),
            );
            let got = BridgeBackend::new(ep)
                .ask_with_id("gw_ask_kind", "hi", &ChatTarget::NewChat, 5_000)
                .unwrap_err();
            assert_eq!(got, expected, "{} {}", code, message);
            host.join().unwrap();
        }
    }

    #[test]
    fn user_stop_messages_are_the_ones_the_extension_tests_pin() {
        // The same file is read by extension/test/userStopMessages.test.ts.
        let pinned: Vec<&str> = include_str!("../../protocol/tests/fixtures/stop_messages.txt")
            .lines()
            .collect();
        assert_eq!(pinned, USER_STOP_MESSAGES);
        // Exact: a longer or embedded text is another failure.
        assert!(!is_user_stop("failed: cancelled by stop later"));
        assert!(!is_user_stop("cancelled by stop."));
    }

    #[test]
    fn bridge_backend_unknown_request_type_means_outdated_bridge() {
        // An old host rejects `stop` (and `status`) as an unknown variant.
        let (ep, host) = fake_host(
            "old-stop",
            serde_json::json!({"type":"error","id":"x","code":"bad_request",
                "message":"invalid request: unknown variant `stop`, expected `ask` or `ping`"}),
        );
        assert_eq!(
            BridgeBackend::new(ep).stop().unwrap_err(),
            BackendError::Outdated
        );
        host.join().unwrap();
        // Any other bad request stays what it was.
        let (ep, host) = fake_host(
            "bad-target",
            serde_json::json!({"type":"error","id":"x","code":"bad_request","message":"bad target"}),
        );
        assert_eq!(
            BridgeBackend::new(ep)
                .ask_with_id("gw_ask_t", "hi", &ChatTarget::NewChat, 5_000)
                .unwrap_err(),
            BackendError::Other("bad target (id моста: gw_ask_t)".into())
        );
        host.join().unwrap();
    }

    #[test]
    fn bridge_backend_stop_uses_a_fresh_id() {
        let (ep, host) = fake_host(
            "stop",
            serde_json::json!({"type":"stopped","id":"x","was_generating":true}),
        );
        BridgeBackend::new(ep).stop().unwrap();
        let request = host.join().unwrap();
        assert_eq!(request["type"], "stop");
        assert!(request["id"].as_str().unwrap().starts_with("gw_stop_"));
        assert_ne!(new_request_id("gw_stop"), new_request_id("gw_stop"));
    }

    #[test]
    fn bridge_backend_stop_ask_names_the_ask() {
        let (ep, host) = fake_host(
            "stop-ask",
            serde_json::json!({"type":"stopped","id":"x","was_generating":true}),
        );
        BridgeBackend::new(ep).stop_ask("gw_ask_1").unwrap();
        let request = host.join().unwrap();
        assert_eq!(request["type"], "stop_ask");
        assert_eq!(request["ask_id"], "gw_ask_1");
        assert!(request["id"].as_str().unwrap().starts_with("gw_stop_"));
    }

    #[test]
    fn bridge_backend_stop_ask_on_a_host_without_it_is_outdated() {
        // Whether the plain stop goes instead is engine::stop_request's call.
        let (ep, host) = fake_host(
            "stop-ask-old",
            serde_json::json!({"type":"error","id":"x","code":"bad_request",
                "message":"invalid request: unknown variant `stop_ask`, expected `ask` or `ping`"}),
        );
        assert_eq!(
            BridgeBackend::new(ep).stop_ask("gw_ask_2"),
            Err(BackendError::Outdated)
        );
        assert_eq!(host.join().unwrap()["type"], "stop_ask");
    }

    #[test]
    fn bridge_backend_records_own_tabs_from_the_last_status_an_extension_answered() {
        let status = |own_tabs: Option<u32>, extension: bool| {
            let mut v = serde_json::json!({"type":"status_result","id":"x",
                "extension":extension,"tab_open":true,"logged_in":true});
            if let Some(n) = own_tabs {
                v["own_tabs"] = serde_json::json!(n);
            }
            v
        };
        let pong = serde_json::json!({"type":"pong","id":"y","host_version":"0.0.1",
            "build":"h1","echo":null,"ts_unix_ms":1});
        let (ep, host) = fake_host_seq(
            "own-tabs",
            vec![
                status(Some(8), true),
                pong.clone(),
                // No extension: says nothing about its tabs.
                status(None, false),
                // An extension without the field (older): none.
                status(None, true),
                pong,
            ],
        );
        let backend = BridgeBackend::new(ep);
        assert_eq!(backend.own_tabs(), None, "nothing before a status");
        assert_eq!(backend.status(), Ok(true));
        assert_eq!(backend.own_tabs(), Some(8));
        assert_eq!(backend.status(), Ok(false));
        assert_eq!(backend.own_tabs(), Some(8));
        assert_eq!(backend.status(), Ok(true));
        assert_eq!(backend.own_tabs(), None);
        host.join().unwrap();
    }

    #[test]
    fn bridge_backend_status_says_whether_an_extension_answered() {
        for (extension, tag) in [(true, "status-ext"), (false, "status-none")] {
            let (ep, host) = fake_host(
                tag,
                serde_json::json!({"type":"status_result","id":"x","extension":extension,
                    "tab_open":false,"logged_in":false}),
            );
            assert_eq!(BridgeBackend::new(ep).status(), Ok(extension));
            let request = host.join().unwrap();
            assert_eq!(request["type"], "status");
            assert!(request["id"].as_str().unwrap().starts_with("gw_status_"));
        }
        // An old host does not know `status`; an old extension leaves the
        // host waiting until its timeout.
        let (ep, host) = fake_host(
            "status-old-host",
            serde_json::json!({"type":"error","id":"x","code":"bad_request",
                "message":"invalid request: unknown variant `status`, expected `ask` or `ping`"}),
        );
        assert_eq!(BridgeBackend::new(ep).status(), Err(BackendError::Outdated));
        host.join().unwrap();
        let (ep, host) = fake_host(
            "status-old-ext",
            serde_json::json!({"type":"error","id":"x","code":"timeout",
                "message":"extension did not respond within 10000 ms"}),
        );
        assert_eq!(BridgeBackend::new(ep).status(), Err(BackendError::Timeout));
        host.join().unwrap();
    }

    #[test]
    fn bridge_backend_without_host_is_not_connected() {
        let err = BridgeBackend::new(endpoint("none"))
            .ask("hi", &ChatTarget::NewChat, 1_000)
            .unwrap_err();
        // Nothing was written: the caller may know the ask never left.
        assert!(matches!(err, BackendError::NotConnected(_)), "{:?}", err);
    }

    /// The card's phrase on Windows; a neutral text about the socket's
    /// permissions elsewhere, where administrators play no part.
    fn assert_permission_hint(message: &str) {
        if cfg!(windows) {
            assert!(
                message.contains("Chrome запущен от администратора?"),
                "{}",
                message
            );
        } else {
            assert!(
                message.contains("нет прав на сокет cgpt-bridge-host"),
                "{}",
                message
            );
            assert!(!message.contains("администратора"), "{}", message);
        }
    }

    #[test]
    fn connect_error_permission_denied_is_no_access_with_a_hint() {
        // Windows: Chrome runs as administrator, so the host's pipe denies
        // the gateway. Nothing was sent, but only a person can fix it: no
        // access (503, not to be resent), not an internal error.
        let err = connect_error(io::Error::new(ErrorKind::PermissionDenied, "denied-detail"));
        let BackendError::NoAccess(message) = err else {
            panic!("expected NoAccess, got {:?}", err);
        };
        assert_permission_hint(&message);
        // The OS error stays in the message for a support log.
        assert!(message.contains("denied-detail"), "{}", message);
        // The other kinds keep their mapping.
        assert!(matches!(
            connect_error(io::Error::from(ErrorKind::NotFound)),
            BackendError::NotConnected(_)
        ));
        assert!(matches!(
            connect_error(io::Error::from(ErrorKind::ConnectionRefused)),
            BackendError::NotConnected(_)
        ));
        let other = connect_error(io::Error::new(ErrorKind::InvalidInput, "boom"));
        assert!(
            matches!(&other, BackendError::Other(m) if m.contains("boom")),
            "{:?}",
            other
        );
    }

    #[cfg(unix)]
    #[test]
    fn bridge_backend_socket_without_permission_is_no_access_with_a_hint() {
        use std::os::unix::fs::PermissionsExt;
        let ep = endpoint("denied");
        let _listener = listen(&ep).unwrap();
        std::fs::set_permissions(&ep, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = BridgeBackend::new(ep.clone()).ask("hi", &ChatTarget::NewChat, 1_000);
        release(&ep);
        match result {
            Err(BackendError::NoAccess(m)) => assert_permission_hint(&m),
            // root ignores the mode bits and connects; nothing to check then.
            Err(BackendError::Unavailable(_)) | Err(BackendError::Timeout) => {}
            other => panic!("unexpected {:?}", other),
        }
    }
}
