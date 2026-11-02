//! The HTTP side: loopback-only listener, bearer auth, routes, SSE and JSON
//! answers, keepalive, and strictly serialized turns.

use std::collections::VecDeque;
use std::io::{self, Cursor, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::backend::{BackendError, ChatBackend};
use crate::config::{Config, MAX_TURN_TIMEOUT_MS};
use crate::cooldown::{self, Cooldown};
use crate::engine::Engine;
use crate::log::log;
use crate::openai::{chunk_json, completion_json, error_json, sse_line, ChatRequest, Reply, Usage};
use crate::session::now_ms;
use crate::title::local_title;

pub const MODEL_CHATGPT: &str = "chatgpt-web";
pub const MODEL_TITLE: &str = "local-title";

/// Upper bound for a request body.
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------- SSE events

/// A whole streamed answer as SSE events (`"data: …\n\n"`): the role chunk, a
/// content chunk (when there is text) and a `tool_calls` chunk (when there are
/// calls), a finish chunk carrying `finish_reason` and `usage`, then
/// `data: [DONE]`.
pub fn sse_reply(id: &str, model: &str, reply: &Reply, usage: Usage) -> Vec<String> {
    let mut out = vec![sse_role_chunk(id, model)];
    let (content, calls): (&str, &[crate::openai::ToolCall]) = match reply {
        Reply::Text(t) => (t, &[]),
        Reply::ToolCalls { content, calls } => (content, calls),
    };
    if !content.is_empty() {
        out.push(sse_line(&chunk_json(
            id,
            model,
            json!({ "content": content }),
            Value::Null,
        )));
    }
    if !calls.is_empty() {
        let calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                json!({
                    "index": i,
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.function.name, "arguments": c.function.arguments },
                })
            })
            .collect();
        out.push(sse_line(&chunk_json(
            id,
            model,
            json!({ "tool_calls": calls }),
            Value::Null,
        )));
    }
    let mut finish = chunk_json(id, model, json!({}), json!(reply.finish_reason()));
    finish["usage"] = json!(usage);
    out.push(sse_line(&finish));
    out.push("data: [DONE]\n\n".to_string());
    out
}

/// The first SSE event of every stream: the assistant role.
fn sse_role_chunk(id: &str, model: &str) -> String {
    sse_line(&chunk_json(
        id,
        model,
        json!({ "role": "assistant", "content": "" }),
        Value::Null,
    ))
}

/// The events that end a stream after it has started, without the role chunk.
fn sse_tail(id: &str, model: &str, reply: &Reply, usage: Usage) -> String {
    sse_reply(id, model, reply, usage)[1..].concat()
}

// -------------------------------------------------------------------- errors

/// HTTP status, machine code and a human message for a backend failure.
///
/// A failure that a resend cannot fix, and that would cost a ChatGPT turn
/// each time, gets 422: the OpenAI SDK and the AI SDK retry only 408, 409,
/// 429 and 5xx (409 is why a stop is not a 409). Whether any failure may be
/// resent is [`may_retry`]'s, not the status's.
fn describe(e: &BackendError) -> (u16, &'static str, String) {
    match e {
        BackendError::Unavailable(m)
        | BackendError::NotConnected(m)
        | BackendError::NoAccess(m) => {
            (503, "extension_unavailable", format!("cgpt-bridge: {}", m))
        }
        BackendError::RateLimited => (
            429,
            "rate_limited",
            "ChatGPT ограничил частоту запросов, повторите позже".to_string(),
        ),
        BackendError::CoolingDown { until_ms } => (
            429,
            "rate_limited",
            format!(
                "ChatGPT ограничил частоту запросов, повторите после {}",
                cooldown::clock(*until_ms)
            ),
        ),
        BackendError::Timeout => (
            504,
            "timeout",
            "ChatGPT не ответил за отведённое время".to_string(),
        ),
        BackendError::QueueTimeout => (
            504,
            "timeout",
            "ChatGPT занят предыдущими запросами: очередь не дошла до этого за отведённое время"
                .to_string(),
        ),
        BackendError::Dom(m) => (
            502,
            "dom_failure",
            format!("Страница ChatGPT ответила не так, как ожидалось: {}", m),
        ),
        BackendError::TabBusy(m) => (
            503,
            "tab_busy",
            format!("Вкладка ChatGPT в cgpt-bridge занята: {}", m),
        ),
        BackendError::Outdated => (
            503,
            "extension_unavailable",
            "Расширение cgpt-bridge устарело: перезагрузите его из extension/dist".to_string(),
        ),
        BackendError::TooLarge(m) => (
            422,
            "answer_too_large",
            format!("Ответ ChatGPT слишком большой для cgpt-bridge: {}", m),
        ),
        BackendError::Cancelled => (
            422,
            "cancelled_by_user",
            "Ответ остановлен пользователем в окне ChatGPT".to_string(),
        ),
        BackendError::Other(m) => (500, "internal_error", m.clone()),
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header is valid")
}

fn json_response(status: u16, body: &Value) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.to_string().into_bytes())
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "application/json"))
}

/// Whether a client may send the request again: only when the message
/// certainly did not reach ChatGPT and nothing waits on a person (a resend
/// after a send is a second turn in the user's chat). A host being restarted
/// before the request sent anything, a turn that never came and an own tab
/// still busy with an earlier answer (the ask typed nothing) are such
/// failures; a missing tab, a sign-in, a socket this user may not open, a
/// rate limit, a stop, a wrong chat or anything unknown is not. The full
/// table is in the gateway spec, 6.1.1.
fn may_retry(e: &BackendError) -> bool {
    match e {
        BackendError::NotConnected(_) | BackendError::QueueTimeout | BackendError::TabBusy(_) => {
            true
        }
        BackendError::Unavailable(_)
        | BackendError::NoAccess(_)
        | BackendError::RateLimited
        | BackendError::CoolingDown { .. }
        | BackendError::Timeout
        | BackendError::Dom(_)
        | BackendError::Outdated
        | BackendError::TooLarge(_)
        | BackendError::Cancelled
        | BackendError::Other(_) => false,
    }
}

/// The error object of a response or an SSE event. A failure not to be
/// resent is an `invalid_request_error`: OpenCode 2.1.4 sorts a streamed
/// error by its status, code and `type`, and retries (up to 10 times, resending
/// the turn) one it does not know, such as `cgpt_gateway`. Some codes are
/// retried by OpenCode whatever the type (spec 6.1.1).
fn backend_error_json(code: &str, message: &str, retry: bool) -> Value {
    let mut body = error_json(code, message);
    if !retry {
        body["error"]["type"] = json!("invalid_request_error");
    }
    body
}

fn respond_error(request: Request, status: u16, code: &str, message: &str) {
    let _ = request.respond(json_response(status, &error_json(code, message)));
}

fn respond_backend_error(request: Request, e: &BackendError) {
    let (status, code, message) = describe(e);
    log(format_args!(
        "cgpt-gateway: {} ({}): {}",
        status, code, message
    ));
    let retry = may_retry(e);
    let mut response = json_response(status, &backend_error_json(code, &message, retry));
    if let BackendError::CoolingDown { until_ms } = e {
        response.add_header(header(
            "Retry-After",
            &retry_after_secs(*until_ms, now_ms()).to_string(),
        ));
    }
    // OpenCode 2.1.4 and the OpenAI SDK obey this header before the status.
    if !retry {
        response.add_header(header("x-should-retry", "false"));
    }
    let _ = request.respond(response);
}

/// Whole seconds from `now_ms` to the end of a pause, rounded up, at least 1.
fn retry_after_secs(until_ms: u128, now_ms: u128) -> u128 {
    until_ms.saturating_sub(now_ms).div_ceil(1000).max(1)
}

// ------------------------------------------------------------ serialized turns

/// A one-seat queue: the value (the engine) is handed to one turn at a time,
/// strictly in the order the requests came (spec 6.1): a request takes its
/// ticket once it has fully arrived (`ticket`) and waits on it later
/// (`wait`), on whatever thread runs the turn. Waiting is bounded, so a
/// request stuck behind a long turn can give up and leave the line.
struct TurnGate<T> {
    state: Mutex<GateState<T>>,
    freed: Condvar,
    /// Times a waiter went to sleep on `freed`, for tests to observe.
    #[cfg(test)]
    parked: std::sync::atomic::AtomicUsize,
}

struct GateState<T> {
    value: Option<T>,
    /// Tickets of the waiting requests, first come first.
    line: VecDeque<u64>,
    next_ticket: u64,
}

struct Turn<'a, T> {
    gate: &'a TurnGate<T>,
    value: Option<T>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<T> TurnGate<T> {
    fn new(value: T) -> TurnGate<T> {
        TurnGate {
            state: Mutex::new(GateState {
                value: Some(value),
                line: VecDeque::new(),
                next_ticket: 0,
            }),
            freed: Condvar::new(),
            #[cfg(test)]
            parked: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Tickets taken so far, whether or not they are still in line.
    #[cfg(test)]
    fn tickets_taken(&self) -> u64 {
        lock(&self.state).next_ticket
    }

    /// Times a waiter has found its turn not yet come and gone to sleep.
    #[cfg(test)]
    fn parked(&self) -> usize {
        self.parked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A place at the end of the line. It must be waited on or left.
    fn ticket(&self) -> u64 {
        let mut state = lock(&self.state);
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        state.line.push_back(ticket);
        ticket
    }

    /// Give up a ticket that will not be waited on.
    fn leave(&self, ticket: u64) {
        lock(&self.state).line.retain(|&t| t != ticket);
        // The next in line may be free to go now.
        self.freed.notify_all();
    }

    #[cfg(test)]
    fn acquire(&self, timeout: Duration) -> Option<Turn<'_, T>> {
        self.wait(self.ticket(), timeout)
    }

    /// Wait until `ticket` is first in line and the value is free; `None`
    /// after `timeout`, and the ticket has left the line.
    fn wait(&self, ticket: u64, timeout: Duration) -> Option<Turn<'_, T>> {
        let start = Instant::now();
        let mut state = lock(&self.state);
        loop {
            if state.line.front() == Some(&ticket) {
                if let Some(value) = state.value.take() {
                    state.line.pop_front();
                    return Some(Turn {
                        gate: self,
                        value: Some(value),
                    });
                }
            }
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                state.line.retain(|&t| t != ticket);
                // The next in line may be free to go now.
                self.freed.notify_all();
                return None;
            }
            #[cfg(test)]
            self.parked
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            state = self
                .freed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl<T> Drop for Turn<'_, T> {
    fn drop(&mut self) {
        lock(&self.gate.state).value = self.value.take();
        // Every waiter checks whether it is first in line.
        self.gate.freed.notify_all();
    }
}

/// Ties a streamed request's handler to its worker, so that a client leaving
/// stops exactly this request's generation and nothing else, and so that the
/// next turn starts only after the stop is done.
///
/// All of it happens under one lock: the worker marks `in_ask` while its ask
/// runs; the handler, on disconnect, sets `cancelled` and, if `in_ask`, takes
/// the stop (`Issuing`) and sends it. After its ask returns the worker clears
/// `in_ask` and waits out an `Issuing` stop before releasing the turn gate.
#[derive(Default)]
struct Cancel {
    state: Mutex<CancelState>,
    stop_done: Condvar,
}

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    in_ask: bool,
    stop: StopPhase,
}

#[derive(Default, PartialEq, Clone, Copy)]
enum StopPhase {
    #[default]
    Idle,
    Issuing,
    Done,
}

/// Marks an issued stop as done when dropped, so a worker waiting in
/// `end_ask` is released even if `stop()` panics.
struct StopDone<'a>(&'a Cancel);

impl Drop for StopDone<'_> {
    fn drop(&mut self) {
        lock(&self.0.state).stop = StopPhase::Done;
        self.0.stop_done.notify_all();
    }
}

impl Cancel {
    /// Handler side: the client is gone.
    fn cancel(&self, backend: &dyn ChatBackend) {
        let mut state = lock(&self.state);
        state.cancelled = true;
        if !(state.in_ask && state.stop == StopPhase::Idle) {
            log("cgpt-gateway: client went away, nothing to stop");
            return;
        }
        state.stop = StopPhase::Issuing;
        drop(state);
        let _done = StopDone(self);
        match backend.stop() {
            Ok(()) => log("cgpt-gateway: client went away, generation stopped"),
            Err(e) => log(format_args!(
                "cgpt-gateway: client went away, stop failed: {:?}",
                e
            )),
        }
    }

    /// Worker side, during the turn: whether the client already left.
    fn is_cancelled(&self) -> bool {
        lock(&self.state).cancelled
    }

    /// Worker side, before the ask: `None` if the client already left.
    fn begin_ask(&self) -> Option<Ask<'_>> {
        let mut state = lock(&self.state);
        if state.cancelled {
            return None;
        }
        state.in_ask = true;
        Some(Ask {
            cancel: self,
            ended: false,
        })
    }

    /// Worker side, after the ask: wait for a stop that is being sent, so the
    /// turn gate is not released into a stop still in flight. Returns whether
    /// the turn was stopped.
    fn end_ask(&self) -> bool {
        let mut state = lock(&self.state);
        state.in_ask = false;
        while state.stop == StopPhase::Issuing {
            state = self
                .stop_done
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        state.stop != StopPhase::Idle
    }
}

/// A running ask; ends it when dropped, so a panicking turn still clears
/// `in_ask` and waits out a stop being sent.
struct Ask<'a> {
    cancel: &'a Cancel,
    ended: bool,
}

impl Ask<'_> {
    /// See [`Cancel::end_ask`].
    fn end(mut self) -> bool {
        self.ended = true;
        self.cancel.end_ask()
    }
}

impl Drop for Ask<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.cancel.end_ask();
        }
    }
}

/// A chat request's ticket in the turn line. It is taken as soon as the
/// request has fully arrived (its body read and checked), so complete
/// requests go in arrival order and a client that stalls while sending its
/// body holds no place; it leaves the line when dropped without being waited
/// on (`local-title`).
struct Place {
    sh: Arc<Shared>,
    ticket: Option<u64>,
}

impl Place {
    fn take(sh: &Arc<Shared>) -> Place {
        Place {
            sh: sh.clone(),
            ticket: Some(sh.gate.ticket()),
        }
    }

    /// The ticket, for waiting on it; the place no longer leaves the line.
    fn into_ticket(mut self) -> u64 {
        self.ticket
            .take()
            .expect("a place holds its ticket until used")
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            self.sh.gate.leave(ticket);
        }
    }
}

struct Shared {
    cfg: Config,
    backend: Box<dyn ChatBackend>,
    /// The engine's pause, checked before a request is queued.
    cooldown: Cooldown,
    gate: TurnGate<Engine>,
    /// Streams whose client was found gone, for tests to observe.
    #[cfg(test)]
    hangups: std::sync::atomic::AtomicUsize,
    /// Chat requests whose body reading has begun, for tests to observe.
    #[cfg(test)]
    body_reads: std::sync::atomic::AtomicUsize,
}

/// Run one request to its answer. `local-title` never touches ChatGPT or the
/// queue; everything else waits its turn (the wait counts against the turn
/// timeout) and then runs the engine.
fn run_turn(
    sh: &Shared,
    place: Place,
    req: &ChatRequest,
    headers: &[(String, String)],
    cancel: &Cancel,
) -> Result<Reply, BackendError> {
    if req.model == MODEL_TITLE {
        return Ok(Reply::Text(local_title(req)));
    }
    let budget = Duration::from_millis(sh.cfg.turn_timeout_ms.min(MAX_TURN_TIMEOUT_MS));
    let started = Instant::now();
    let mut turn = sh
        .gate
        .wait(place.into_ticket(), budget)
        .ok_or(BackendError::QueueTimeout)?;
    let remaining = budget.saturating_sub(started.elapsed()).as_millis() as u64;
    if remaining < sh.cfg.effective_min_ask_budget_ms() {
        // Queueing ate the budget: an ask would be cut off mid-answer.
        return Err(BackendError::QueueTimeout);
    }
    // Dropped before `turn`: the gate is released after the ask has ended.
    let Some(ask) = cancel.begin_ask() else {
        return Err(BackendError::Other("запрос отменён клиентом".to_string()));
    };
    let engine = turn.value.as_mut().expect("turn holds the engine");
    let result = engine.handle(headers, req, &*sh.backend, remaining, &|| {
        cancel.is_cancelled()
    });
    // The turn is released (end of this function) only after a stop that is
    // being sent has finished; the result of a stopped turn is dropped, and
    // its session is marked interrupted even if the answer had arrived.
    if ask.end() {
        engine.interrupt_last_turn();
        return Err(BackendError::Other("запрос остановлен".to_string()));
    }
    result
}

// -------------------------------------------------------------------- server

pub struct Running {
    #[cfg_attr(not(test), allow(dead_code))]
    server: Arc<Server>,
    #[cfg(test)]
    shared: Arc<Shared>,
    addr: SocketAddr,
    thread: JoinHandle<()>,
}

impl Running {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Block until the accept loop ends.
    pub fn wait(self) {
        let _ = self.thread.join();
    }

    /// Chat requests that have taken their place in the turn line so far.
    #[cfg(test)]
    pub fn tickets_taken(&self) -> u64 {
        self.shared.gate.tickets_taken()
    }

    /// Chat requests whose handler has begun to read the body.
    #[cfg(test)]
    pub fn body_reads(&self) -> usize {
        self.shared
            .body_reads
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Streams whose client was found gone (and its request cancelled).
    #[cfg(test)]
    pub fn hangups(&self) -> usize {
        self.shared
            .hangups
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn shutdown(self) {
        self.server.unblock();
        self.wait();
    }
}

/// Bind `127.0.0.1:<cfg.port>` (never another interface) and serve in the
/// background.
pub fn start(cfg: Config, backend: Box<dyn ChatBackend>, engine: Engine) -> io::Result<Running> {
    // The same minimum guards every part of a turn, not only the first.
    let engine = engine.with_min_ask_budget_ms(cfg.effective_min_ask_budget_ms());
    let server = Server::http(("127.0.0.1", cfg.port))
        .map_err(|e| io::Error::other(e.to_string()))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| io::Error::other("listener has no IP address"))?;
    let server = Arc::new(server);
    let shared = Arc::new(Shared {
        cfg,
        backend,
        cooldown: engine.cooldown(),
        gate: TurnGate::new(engine),
        #[cfg(test)]
        hangups: std::sync::atomic::AtomicUsize::new(0),
        #[cfg(test)]
        body_reads: std::sync::atomic::AtomicUsize::new(0),
    });
    #[cfg(test)]
    let test_shared = shared.clone();
    let accept_server = server.clone();
    let thread = thread::spawn(move || {
        for request in accept_server.incoming_requests() {
            let shared = shared.clone();
            thread::spawn(move || handle(&shared, request));
        }
    });
    Ok(Running {
        server,
        #[cfg(test)]
        shared: test_shared,
        addr,
        thread,
    })
}

pub fn serve(cfg: Config, backend: Box<dyn ChatBackend>, engine: Engine) -> io::Result<()> {
    let running = start(cfg, backend, engine)?;
    log(format_args!(
        "cgpt-gateway: listening on http://{}",
        running.addr()
    ));
    running.wait();
    Ok(())
}

fn request_header(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

/// Constant-time comparison of two byte strings of any length.
fn token_matches(given: &str, expected: &str) -> bool {
    let (a, b) = (given.as_bytes(), expected.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

fn authorized(request: &Request, token: &str) -> bool {
    request_header(request, "Authorization")
        .and_then(|v| {
            let (scheme, given) = v.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then(|| given.trim().to_string())
        })
        .map(|given| token_matches(&given, token))
        .unwrap_or(false)
}

/// All request headers as (name, value) pairs.
fn request_headers(request: &Request) -> Vec<(String, String)> {
    request
        .headers()
        .iter()
        .map(|h| {
            (
                h.field.as_str().as_str().to_string(),
                h.value.as_str().to_string(),
            )
        })
        .collect()
}

fn handle(sh: &Arc<Shared>, request: Request) {
    if !authorized(&request, &sh.cfg.token) {
        return respond_error(
            request,
            401,
            "unauthorized",
            "нужен заголовок Authorization: Bearer <token>",
        );
    }
    let path = request.url().split('?').next().unwrap_or("").to_string();
    match (request.method().clone(), path.as_str()) {
        (Method::Get, "/v1/models") => models(request),
        (Method::Post, "/v1/chat/completions") => chat(sh, request),
        _ => respond_error(request, 404, "not_found", "неизвестный маршрут"),
    }
}

fn models(request: Request) {
    let model =
        |id: &str| json!({ "id": id, "object": "model", "created": 0, "owned_by": "cgpt-gateway" });
    let body = json!({ "object": "list", "data": [model(MODEL_CHATGPT), model(MODEL_TITLE)] });
    let _ = request.respond(json_response(200, &body));
}

fn completion_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "chatcmpl-{:x}{:04x}",
        ms,
        COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff
    )
}

fn chat(sh: &Arc<Shared>, mut request: Request) {
    let headers = request_headers(&request);
    #[cfg(test)]
    sh.body_reads
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut body = String::new();
    let read = request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut body);
    if let Err(e) = read {
        return respond_error(
            request,
            400,
            "bad_request",
            &format!("тело запроса не прочитано: {}", e),
        );
    }
    if body.len() as u64 > MAX_BODY_BYTES {
        return respond_error(
            request,
            413,
            "payload_too_large",
            "тело запроса слишком большое",
        );
    }
    let req: ChatRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return respond_error(
                request,
                400,
                "bad_request",
                &format!("тело запроса не разобрано: {}", e),
            )
        }
    };
    // The raw text (up to MAX_BODY_BYTES) is not needed once parsed: do not
    // keep it next to `req` for the whole turn.
    drop(body);
    if req.model != MODEL_CHATGPT && req.model != MODEL_TITLE {
        let message = format!(
            "модель «{}» неизвестна, доступны {} и {}",
            req.model, MODEL_CHATGPT, MODEL_TITLE
        );
        return respond_error(request, 404, "model_not_found", &message);
    }
    if !req.messages.iter().any(|m| m.role == "user") {
        return respond_error(
            request,
            400,
            "bad_request",
            "в запросе нет сообщения пользователя",
        );
    }
    // Before the queue and the stream: a request during a pause gets a real
    // HTTP 429 at once.
    if req.model == MODEL_CHATGPT {
        if let Err(e) = sh.cooldown.check(now_ms()) {
            return respond_backend_error(request, &e);
        }
    }
    // The request is complete: its place in the turn line.
    let place = Place::take(sh);
    if req.wants_stream() {
        chat_stream(sh, request, req, headers, place);
    } else {
        let cancel = Cancel::default();
        match run_turn(sh, place, &req, &headers, &cancel) {
            Ok(reply) => {
                let usage = Usage::estimate(req.prompt_chars(), reply.completion_chars());
                let body = completion_json(&completion_id(), &req.model, &reply, usage);
                let _ = request.respond(json_response(200, &body));
            }
            Err(e) => respond_backend_error(request, &e),
        }
    }
}

fn sse_headers() -> Vec<Header> {
    vec![
        header("Content-Type", "text/event-stream; charset=utf-8"),
        header("Cache-Control", "no-cache"),
    ]
}

/// A streamed answer. The turn runs on a worker thread; this thread decides
/// how to answer. A request that fails within `first_byte_wait_ms` (no
/// extension, a cooldown) still gets a plain HTTP error. Otherwise the SSE
/// stream starts, with keepalive comments until the answer is there.
fn chat_stream(
    sh: &Arc<Shared>,
    request: Request,
    req: ChatRequest,
    headers: Vec<(String, String)>,
    place: Place,
) {
    let id = completion_id();
    let model = req.model.clone();
    let prompt_chars = req.prompt_chars();
    let cancel = Arc::new(Cancel::default());
    let (tx, rx) = mpsc::channel();
    {
        let (sh, cancel) = (sh.clone(), cancel.clone());
        thread::spawn(move || {
            let _ = tx.send(run_turn(&sh, place, &req, &headers, &cancel));
        });
    }

    let whole = |request: Request, result: Result<Reply, BackendError>| match result {
        Err(e) => respond_backend_error(request, &e),
        Ok(reply) => {
            let usage = Usage::estimate(prompt_chars, reply.completion_chars());
            let body = sse_reply(&id, &model, &reply, usage).concat().into_bytes();
            let mut response = Response::from_data(body);
            for h in sse_headers() {
                response.add_header(h);
            }
            let _ = request.respond(response);
        }
    };
    // The turn may have sent its message before it crashed.
    let crashed = |request: Request| {
        respond_backend_error(
            request,
            &BackendError::Other("обработка запроса завершилась аварийно".to_string()),
        )
    };
    match rx.recv_timeout(Duration::from_millis(sh.cfg.first_byte_wait_ms)) {
        Ok(result) => whole(request, result),
        Err(RecvTimeoutError::Disconnected) => crashed(request),
        Err(RecvTimeoutError::Timeout) if keeps_alive_without_chunks(&request) => {
            // tiny_http cannot close this connection after a raw stream, and
            // without chunks the client could not see where it ends: the
            // whole answer goes out at once, with its length.
            match rx.recv() {
                Ok(result) => whole(request, result),
                Err(_) => crashed(request),
            }
        }
        Err(RecvTimeoutError::Timeout) => {
            let live = LiveStream {
                id,
                model,
                prompt_chars,
                keepalive: sh.cfg.keepalive_ms,
            };
            if live.run(request, &rx).is_err() {
                // A write failed: the client is gone.
                cancel.cancel(&*sh.backend);
                #[cfg(test)]
                sh.hangups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

/// An HTTP/1.0 client that asked to keep the connection open.
fn keeps_alive_without_chunks(request: &Request) -> bool {
    *request.http_version() < (1u8, 1u8)
        && request_header(request, "Connection")
            .is_some_and(|v| v.to_ascii_lowercase().contains("keep-alive"))
}

/// The body of a stream that has already started.
struct LiveStream {
    id: String,
    model: String,
    prompt_chars: usize,
    keepalive: u64,
}

impl LiveStream {
    /// Write the head and the role chunk, keepalive comments while the worker
    /// is busy, then the answer and `[DONE]`. A failure now can no longer
    /// change the HTTP status, so it goes out as an OpenAI error event and
    /// `[DONE]`; it is never shown as assistant text, which OpenCode would
    /// keep as something ChatGPT said. Returns `Err` on a failed write.
    fn run(&self, request: Request, rx: &Receiver<Result<Reply, BackendError>>) -> io::Result<()> {
        let mut out = SseWriter::start(request);
        out.head()?;
        out.event(&sse_role_chunk(&self.id, &self.model))?;
        let keepalive = Duration::from_millis(self.keepalive.max(1));
        let result = loop {
            match rx.recv_timeout(keepalive) {
                Ok(result) => break result,
                Err(RecvTimeoutError::Timeout) => out.event(": keepalive\n\n")?,
                Err(RecvTimeoutError::Disconnected) => {
                    break Err(BackendError::Other(
                        "обработка запроса завершилась аварийно".to_string(),
                    ))
                }
            }
        };
        match result {
            Ok(reply) => {
                let usage = Usage::estimate(self.prompt_chars, reply.completion_chars());
                out.event(&sse_tail(&self.id, &self.model, &reply, usage))?;
            }
            Err(e) => {
                let (status, code, message) = describe(&e);
                let code = stream_code(code);
                log(format_args!(
                    "cgpt-gateway: {} ({}) after the stream started: {}",
                    status, code, message
                ));
                out.event(&sse_error_events(code, &message, may_retry(&e)))?;
            }
        }
        out.finish()
    }
}

/// The code of an error event in a started stream. OpenCode 2.1.4 sorts a
/// streamed error by its `code` before its `type`, and retries a server code
/// such as `internal_error`, or any code containing `unavailable`, whatever
/// the type says (spec 6.1.1), so those have other names here. An HTTP answer
/// keeps its code: `x-should-retry` decides there.
fn stream_code(code: &'static str) -> &'static str {
    match code {
        "extension_unavailable" => "bridge_offline",
        "internal_error" => "bridge_internal",
        other => other,
    }
}

/// The events that end a stream with an error: an OpenAI error object, then
/// `[DONE]`.
fn sse_error_events(code: &str, message: &str, retry: bool) -> String {
    format!(
        "{}data: [DONE]\n\n",
        sse_line(&backend_error_json(code, message, retry))
    )
}

/// Writes a live SSE response by hand: tiny_http's own response path buffers
/// a chunked body and would hold events back. Every event is framed as one
/// HTTP chunk and flushed at once. HTTP/1.0 clients cannot chunk: they get
/// the raw stream and the connection ends it (those that ask for keep-alive
/// never get here, see `keeps_alive_without_chunks`).
struct SseWriter {
    out: Box<dyn Write + Send>,
    chunked: bool,
}

impl SseWriter {
    fn start(request: Request) -> SseWriter {
        let chunked = *request.http_version() >= (1u8, 1u8);
        SseWriter {
            out: request.into_writer(),
            chunked,
        }
    }

    fn head(&mut self) -> io::Result<()> {
        let head = if self.chunked {
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
             Cache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n"
        } else {
            "HTTP/1.0 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
             Cache-Control: no-cache\r\nConnection: close\r\n\r\n"
        };
        self.out.write_all(head.as_bytes())?;
        self.out.flush()
    }

    fn event(&mut self, text: &str) -> io::Result<()> {
        if self.chunked {
            write!(self.out, "{:x}\r\n", text.len())?;
            self.out.write_all(text.as_bytes())?;
            self.out.write_all(b"\r\n")?;
        } else {
            self.out.write_all(text.as_bytes())?;
        }
        self.out.flush()
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.chunked {
            self.out.write_all(b"0\r\n\r\n")?;
        }
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{FakeBackend, IN_TARGET_CHAT};
    use crate::openai::{FunctionCall, ToolCall};
    use crate::session::tests::temp_dir;
    use crate::session::Sessions;
    use cgpt_bridge_protocol::ChatTarget;
    use std::io::Write;
    use std::net::TcpStream;

    const TOKEN: &str = "test-token";
    /// The chat every single-chat test answers from.
    const CHAT: &str = "https://chatgpt.com/c/test";

    fn start_with(fake: &Arc<FakeBackend>, tweak: impl FnOnce(&mut Config)) -> Running {
        let mut cfg = Config::with_token(TOKEN.to_string());
        cfg.port = 0;
        tweak(&mut cfg);
        let dir = temp_dir("http");
        let engine = Engine::new(
            Sessions::new(dir.clone()),
            cfg.allow_tools.clone(),
            Cooldown::new(dir.join("cooldown.json")),
        );
        start(cfg, Box::new(fake.clone()), engine).unwrap()
    }

    struct Resp {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Resp {
        fn text(&self) -> String {
            String::from_utf8(self.body.clone()).expect("response body is valid UTF-8")
        }
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).expect("response body is JSON")
        }
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
        /// Parsed `data:` events of an SSE body (without `[DONE]`), and
        /// whether `[DONE]` was the last event.
        fn sse(&self) -> (Vec<Value>, bool) {
            let text = self.text();
            let mut events = Vec::new();
            let mut done = false;
            for block in text.split("\n\n") {
                if let Some(data) = block.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        done = true;
                    } else {
                        events.push(serde_json::from_str(data).expect("SSE data is JSON"));
                        done = false;
                    }
                }
            }
            (events, done)
        }
    }

    fn decode_chunked(mut raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let eol = raw
                .windows(2)
                .position(|w| w == b"\r\n")
                .expect("chunk size line");
            let size = usize::from_str_radix(std::str::from_utf8(&raw[..eol]).unwrap().trim(), 16)
                .unwrap();
            raw = &raw[eol + 2..];
            if size == 0 {
                return out;
            }
            out.extend_from_slice(&raw[..size]);
            raw = &raw[size + 2..];
        }
    }

    fn send_raw(
        port: u16,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n",
            method, path
        );
        for (k, v) in headers {
            head.push_str(&format!("{}: {}\r\n", k, v));
        }
        let bytes = body.unwrap_or("").as_bytes();
        if body.is_some() {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                bytes.len()
            ));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(bytes).unwrap();
        stream
    }

    fn read_response(mut stream: TcpStream) -> Resp {
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        parse_response(raw)
    }

    /// Read from a live stream into `raw` until `done` holds for what has
    /// arrived so far. The bounds (10 s for one read, 20 s in all) only turn
    /// a hang into a failure: the events awaited here come from the gateway,
    /// not from a timer of the test.
    fn read_until(stream: &mut TcpStream, raw: &mut Vec<u8>, done: impl Fn(&str) -> bool) {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut buf = [0u8; 4096];
        while !done(&String::from_utf8_lossy(raw)) {
            assert!(
                Instant::now() < deadline,
                "timed out reading the stream: {:?}",
                String::from_utf8_lossy(raw)
            );
            let n = stream.read(&mut buf).expect("the stream went quiet");
            assert!(
                n > 0,
                "stream ended early: {:?}",
                String::from_utf8_lossy(raw)
            );
            raw.extend_from_slice(&buf[..n]);
        }
    }

    /// The rest of a stream, after `read_until`, as one response.
    fn finish_response(mut stream: TcpStream, mut raw: Vec<u8>) -> Resp {
        stream.set_read_timeout(None).unwrap();
        stream.read_to_end(&mut raw).unwrap();
        parse_response(raw)
    }

    /// Let wall-clock time pass, for a test whose subject is a time budget
    /// (not for putting events in order).
    fn let_time_pass(d: Duration) {
        thread::sleep(d);
    }

    fn parse_response(raw: Vec<u8>) -> Resp {
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header end");
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut lines = head.lines();
        let status = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let body = &raw[split + 4..];
        let chunked = headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
        });
        Resp {
            status,
            headers,
            body: if chunked {
                decode_chunked(body)
            } else {
                body.to_vec()
            },
        }
    }

    fn call(port: u16, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> Resp {
        let auth = token.map(|t| format!("Bearer {}", t));
        let mut headers = Vec::new();
        if let Some(a) = auth.as_deref() {
            headers.push(("Authorization", a));
        }
        read_response(send_raw(port, method, path, &headers, body))
    }

    fn chat_body(model: &str, user: &str, stream: bool) -> String {
        json!({
            "model": model,
            "messages": [
                { "role": "system", "content": "rules" },
                { "role": "user", "content": user },
            ],
            "stream": stream,
            "stream_options": { "include_usage": true },
        })
        .to_string()
    }

    fn chat(port: u16, model: &str, user: &str, stream: bool) -> Resp {
        call(
            port,
            "POST",
            "/v1/chat/completions",
            Some(TOKEN),
            Some(&chat_body(model, user, stream)),
        )
    }

    /// The last user message in a text sent to ChatGPT.
    fn user_of(sent: &str) -> String {
        let at = sent.rfind("[пользователь]\n").expect("a user message") + "[пользователь]\n".len();
        let rest = &sent[at..];
        rest[..rest.find("\n\n").unwrap_or(rest.len())].to_string()
    }

    /// The user message of every text sent to ChatGPT.
    fn asked(fake: &FakeBackend) -> Vec<String> {
        fake.sent_texts().iter().map(|t| user_of(t)).collect()
    }

    fn assistant_text(events: &[Value]) -> String {
        events
            .iter()
            .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
            .collect()
    }

    // ---- sse_reply shape

    fn usage() -> Usage {
        Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        }
    }

    fn parse_events(events: &[String]) -> Vec<Value> {
        events
            .iter()
            .filter(|e| *e != "data: [DONE]\n\n")
            .map(|e| {
                assert!(
                    e.starts_with("data: ") && e.ends_with("\n\n"),
                    "bad event {:?}",
                    e
                );
                serde_json::from_str(&e["data: ".len()..e.len() - 2]).unwrap()
            })
            .collect()
    }

    #[test]
    fn stream_reply_shape_text() {
        let events = sse_reply(
            "chatcmpl-1",
            "chatgpt-web",
            &Reply::Text("Привет 🎉".into()),
            usage(),
        );
        assert_eq!(events.last().unwrap(), "data: [DONE]\n\n");
        let json = parse_events(&events);
        assert_eq!(json.len(), 3, "role, content, finish");
        assert_eq!(json[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(json[0]["object"], "chat.completion.chunk");
        assert_eq!(json[0]["id"], "chatcmpl-1");
        assert_eq!(json[0]["model"], "chatgpt-web");
        assert_eq!(json[1]["choices"][0]["delta"]["content"], "Привет 🎉");
        assert_eq!(json[2]["choices"][0]["finish_reason"], "stop");
        assert_eq!(json[2]["usage"]["total_tokens"], 15);
        assert_eq!(json[2]["usage"]["prompt_tokens"], 10);
    }

    #[test]
    fn stream_reply_shape_tool_calls() {
        let reply = Reply::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall {
                id: "call_abcd1234_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                },
            }],
        };
        let events = sse_reply("chatcmpl-2", "chatgpt-web", &reply, usage());
        let json = parse_events(&events);
        assert_eq!(
            json.len(),
            3,
            "role, tool_calls, finish (no empty content chunk)"
        );
        assert_eq!(json[0]["choices"][0]["delta"]["role"], "assistant");
        let call = &json[1]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "call_abcd1234_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "read");
        // arguments is a string holding JSON, as OpenAI defines it
        let args = call["function"]["arguments"]
            .as_str()
            .expect("arguments is a string");
        assert_eq!(
            serde_json::from_str::<Value>(args).unwrap(),
            json!({"path": "a.txt"})
        );
        assert_eq!(json[2]["choices"][0]["finish_reason"], "tool_calls");
        assert!(json[2]["usage"].is_object());
        assert_eq!(events.last().unwrap(), "data: [DONE]\n\n");
    }

    #[test]
    fn non_stream_reply_shape() {
        let fake = FakeBackend::new();
        fake.push_reply(Ok(("Ответ 🎉".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |_| {});
        let resp = chat(gw.port(), "chatgpt-web", &"x".repeat(40), false);
        assert_eq!(resp.status, 200);
        assert!(resp
            .header("content-type")
            .unwrap()
            .starts_with("application/json"));
        let v = resp.json();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["model"], "chatgpt-web");
        assert_eq!(v["choices"][0]["message"]["role"], "assistant");
        assert_eq!(v["choices"][0]["message"]["content"], "Ответ 🎉");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        // characters / 4: the prompt is "rules" + 40 x's
        assert_eq!(v["usage"]["prompt_tokens"], 11);
        assert_eq!(v["usage"]["completion_tokens"], 1);
        assert_eq!(v["usage"]["total_tokens"], 12);

        let calls = completion_json(
            "id",
            "chatgpt-web",
            &Reply::ToolCalls {
                content: String::new(),
                calls: vec![ToolCall {
                    id: "call_x_1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "glob".into(),
                        arguments: "{}".into(),
                    },
                }],
            },
            usage(),
        );
        assert_eq!(calls["choices"][0]["finish_reason"], "tool_calls");
        assert!(calls["choices"][0]["message"]["content"].is_null());
        assert_eq!(
            calls["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
        gw.shutdown();
    }

    #[test]
    fn stream_over_http_has_role_content_finish_usage_done() {
        let fake = FakeBackend::new();
        fake.push_reply(Ok(("готово".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |_| {});
        let resp = chat(gw.port(), "chatgpt-web", "привет", true);
        assert_eq!(resp.status, 200);
        assert!(resp
            .header("content-type")
            .unwrap()
            .starts_with("text/event-stream"));
        let (events, done) = resp.sse();
        assert!(done, "last event is [DONE]");
        assert_eq!(
            events.first().unwrap()["choices"][0]["delta"]["role"],
            "assistant"
        );
        assert_eq!(assistant_text(&events), "готово");
        let last = events.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert!(last["usage"]["total_tokens"].is_u64());
        assert_eq!(asked(&fake), ["привет"]);
        assert_eq!(fake.sent.lock().unwrap()[0].1, ChatTarget::NewChat);
        gw.shutdown();
    }

    // ---- config / binding / auth

    #[test]
    fn binds_only_loopback() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        let addr = gw.addr();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert!(!addr.ip().is_unspecified());
        gw.shutdown();
    }

    #[test]
    fn missing_or_wrong_token_401() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        let port = gw.port();
        for (method, path, body) in [
            ("GET", "/v1/models", None),
            (
                "POST",
                "/v1/chat/completions",
                Some(chat_body("chatgpt-web", "hi", false)),
            ),
            ("GET", "/nowhere", None),
        ] {
            for token in [
                None,
                Some("wrong"),
                Some(""),
                Some("test-token-and-more"),
                Some("test-toke"),
            ] {
                let resp = call(port, method, path, token, body.as_deref());
                assert_eq!(resp.status, 401, "{} {} with {:?}", method, path, token);
                assert_eq!(resp.json()["error"]["code"], "unauthorized");
                assert_eq!(resp.json()["error"]["type"], "cgpt_gateway");
            }
        }
        // Not a Bearer scheme.
        let resp = read_response(send_raw(
            port,
            "GET",
            "/v1/models",
            &[("Authorization", "Basic test-token")],
            None,
        ));
        assert_eq!(resp.status, 401);
        assert!(
            fake.sent.lock().unwrap().is_empty(),
            "nothing reached the backend"
        );
        assert_eq!(
            call(port, "GET", "/v1/models", Some(TOKEN), None).status,
            200
        );
        gw.shutdown();
    }

    #[test]
    fn models_lists_chatgpt_web_and_local_title() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        let resp = call(gw.port(), "GET", "/v1/models", Some(TOKEN), None);
        assert_eq!(resp.status, 200);
        let v = resp.json();
        assert_eq!(v["object"], "list");
        let ids: Vec<&str> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["chatgpt-web", "local-title"]);
        gw.shutdown();
    }

    #[test]
    fn unknown_route_model_and_bad_bodies() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        let port = gw.port();
        assert_eq!(
            call(port, "GET", "/v1/nothing", Some(TOKEN), None).status,
            404
        );
        assert_eq!(
            call(
                port,
                "POST",
                "/v1/chat/completions",
                Some(TOKEN),
                Some("{not json")
            )
            .status,
            400
        );
        let resp = chat(port, "gpt-4", "hi", false);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.json()["error"]["code"], "model_not_found");
        let only_system =
            json!({"model":"chatgpt-web","messages":[{"role":"system","content":"s"}]}).to_string();
        assert_eq!(
            call(
                port,
                "POST",
                "/v1/chat/completions",
                Some(TOKEN),
                Some(&only_system)
            )
            .status,
            400
        );
        assert!(fake.sent.lock().unwrap().is_empty());
        gw.shutdown();
    }

    // ---- local title

    #[test]
    fn local_title_answers_without_backend() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        let prompt = "Here is the first message:\n\n<message>\n\"Прочитай a.txt и скажи, что в нём\"\n</message>";
        let resp = chat(gw.port(), "local-title", prompt, false);
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.json()["choices"][0]["message"]["content"],
            "Прочитай a.txt и скажи, что в нём"
        );

        let resp = chat(gw.port(), "local-title", prompt, true);
        let (events, done) = resp.sse();
        assert!(done);
        assert_eq!(assistant_text(&events), "Прочитай a.txt и скажи, что в нём");
        assert!(
            fake.sent.lock().unwrap().is_empty(),
            "ChatGPT was not asked"
        );
        gw.shutdown();
    }

    // ---- keepalive, disconnect

    #[test]
    fn keepalive_comment_every_interval() {
        let fake = FakeBackend::new();
        // The answer is held until the client has seen three keepalives.
        fake.close_gate();
        fake.push_reply(Ok(("late answer".into(), Some(CHAT.into()))));
        // The 10 s / 25 s of production, scaled down 100x.
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 100;
            c.first_byte_wait_ms = 50;
        });
        let body = chat_body("chatgpt-web", "hi", true);
        let mut stream = send_raw(
            gw.port(),
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(&body),
        );
        let sent_at = Instant::now();
        let mut raw = Vec::new();
        read_until(&mut stream, &mut raw, |seen| {
            seen.matches(": keepalive\n\n").count() >= 3
        });
        let three = sent_at.elapsed();
        fake.open_gate();
        // The interval is the configured one: the third keepalive cannot
        // come before three intervals have passed, and a gateway that
        // ignored the setting (the default is 2 s) would need over 6 s. The
        // upper bound is ten times what a quiet runner takes.
        assert!(three >= Duration::from_millis(300), "{:?}", three);
        assert!(three <= Duration::from_secs(5), "{:?}", three);
        let resp = finish_response(stream, raw);
        assert_eq!(resp.status, 200);
        let text = resp.text();
        // Role chunk first, comments in between, answer after them.
        let role = text.find("\"role\":\"assistant\"").unwrap();
        let first_keepalive = text.find(": keepalive").unwrap();
        let answer = text.find("late answer").unwrap();
        assert!(role < first_keepalive && first_keepalive < answer);
        let (events, done) = resp.sse();
        assert!(done);
        assert_eq!(assistant_text(&events), "late answer");
        gw.shutdown();
    }

    /// Wait for `cond`, which watches something the gateway or the fake has
    /// done (a poll, not a pause: the deadline only turns a hang into a
    /// failure).
    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {}", what);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn client_disconnect_calls_backend_stop() {
        let fake = FakeBackend::new();
        // The answer never comes by itself: only `stop` ends the ask.
        fake.close_gate();
        fake.push_reply(Ok(("never read".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 20;
        });
        let body = chat_body("chatgpt-web", "hi", true);
        let mut stream = send_raw(
            gw.port(),
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(&body),
        );
        // The role chunk must arrive while the answer is still pending:
        // events are flushed live, not held in tiny_http's buffer. Then hang
        // up.
        let mut raw = Vec::new();
        read_until(&mut stream, &mut raw, |seen| {
            seen.contains("\"role\":\"assistant\"")
        });
        // The ask is under way: a hang-up now must stop it.
        assert!(fake.wait_for_asks(1), "the ask started");
        drop(stream);
        wait_for("stop after disconnect", || {
            fake.stops.load(Ordering::SeqCst) == 1
        });
        gw.shutdown();
    }

    #[test]
    fn stop_after_a_client_that_closed_a_live_stream_cleanly_comes_within_7_s() {
        // The settings the gateway runs with. The client reads everything the
        // stream has sent and closes the socket (a FIN, not a reset): the
        // first keepalive write after that still succeeds and only provokes
        // the reset, the second one fails. Worst case two intervals (4 s with
        // the default 2 s); the old 10 s default is far above the bound.
        let fake = FakeBackend::new();
        // The answer never comes by itself: only `stop` ends the ask.
        fake.close_gate();
        fake.push_reply(Ok(("never read".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |_| {});
        let body = chat_body("chatgpt-web", "hi", true);
        let mut stream = send_raw(
            gw.port(),
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(&body),
        );
        // The stream starts after first_byte_wait_ms (500 ms); then take all
        // of the head and the role chunk (a chunk ends in CRLF), so that
        // nothing is left unread.
        let mut raw = Vec::new();
        read_until(&mut stream, &mut raw, |seen| {
            seen.contains("\"role\":\"assistant\"") && seen.ends_with("\n\n\r\n")
        });
        let left = Instant::now();
        drop(stream);
        let deadline = left + Duration::from_secs(8);
        while fake.stops.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no stop after the client left");
            thread::sleep(Duration::from_millis(10));
        }
        let latency = left.elapsed();
        gw.shutdown();
        assert!(latency <= Duration::from_secs(7), "{:?}", latency);
    }

    #[test]
    fn disconnect_of_a_queued_request_does_not_stop_the_running_one() {
        let fake = FakeBackend::new();
        // A's ask is held until B has been queued and has hung up.
        fake.close_gate();
        fake.push_reply(Ok(("first".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 20;
        });
        let port = gw.port();
        let running = thread::spawn(move || chat(port, "chatgpt-web", "A", false));
        assert!(fake.wait_for_asks(1), "A reached the backend");

        let body = chat_body("chatgpt-web", "B", true);
        let mut queued = send_raw(
            port,
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(&body),
        );
        let mut buf = [0u8; 64];
        assert!(
            queued.read(&mut buf).unwrap() > 0,
            "B's stream started while waiting"
        );
        drop(queued);
        // The gateway has found B gone (its keepalive write failed) while A
        // is still running.
        wait_for("B's hang-up to be noticed", || gw.hangups() == 1);
        assert_eq!(
            fake.stops.load(Ordering::SeqCst),
            0,
            "A's generation must not be stopped for B"
        );
        fake.open_gate();
        assert_eq!(running.join().unwrap().status, 200);
        // B's worker takes its turn after A, sees the client is gone and
        // never asks: C, sent now, is the second and last ask.
        fake.push_reply(Ok(("c answer".into(), Some(CHAT.into()))));
        assert_eq!(chat(port, "chatgpt-web", "C", false).status, 200);
        assert_eq!(asked(&fake), ["A", "C"], "B must never reach ChatGPT");
        assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
        gw.shutdown();
    }

    // ---- errors

    /// Failures that a retry cannot fix and that would cost a ChatGPT turn
    /// each: a 4xx status that the OpenAI SDK (408, 409, 429, >= 500) and the
    /// AI SDK behind OpenCode (the same list) do not retry, and an
    /// `x-should-retry: false` for the clients that read it.
    #[test]
    fn answer_too_large_and_user_stop_are_not_retried_by_clients() {
        let cases = [
            (
                BackendError::TooLarge("ответ 2000000 байт".into()),
                "answer_too_large",
            ),
            (BackendError::Cancelled, "cancelled_by_user"),
        ];
        for stream in [false, true] {
            let fake = FakeBackend::new();
            // A long first-byte window: the HTTP error must win even on a loaded machine.
            let gw = start_with(&fake, |c| c.first_byte_wait_ms = 10_000);
            for (error, code) in cases.clone() {
                fake.push_reply(Err(error));
                let resp = chat(gw.port(), "chatgpt-web", "hi", stream);
                assert_eq!(resp.status, 422, "stream={} code={}", stream, code);
                assert!(![408, 409, 429].contains(&resp.status) && resp.status < 500);
                assert_eq!(resp.header("x-should-retry"), Some("false"));
                let v = resp.json();
                assert_eq!(v["error"]["code"], code);
                // OpenCode 2.1.4 reads `error.type`; an unknown one is retried.
                assert_eq!(v["error"]["type"], "invalid_request_error");
                assert!(!v["error"]["message"].as_str().unwrap().is_empty());
            }
            // One ask per request: nothing was resent.
            assert_eq!(fake.sent_texts().len(), 2, "stream={}", stream);
            gw.shutdown();
        }
    }

    /// A streamed request whose ask fails only after the client has seen the
    /// role chunk: the ask is held until then. The fake's gate must be closed.
    fn chat_failing_after_the_stream_started(fake: &FakeBackend, port: u16) -> Resp {
        failing_after_the_stream_started(port, &chat_body("chatgpt-web", "hi", true), || {
            fake.open_gate()
        })
    }

    /// Send a streamed `body` and call `let_go` once the client has seen the
    /// role chunk; the whole response.
    fn failing_after_the_stream_started(port: u16, body: &str, let_go: impl FnOnce()) -> Resp {
        let mut stream = send_raw(
            port,
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(body),
        );
        let mut raw = Vec::new();
        read_until(&mut stream, &mut raw, |seen| {
            seen.contains("\"role\":\"assistant\"")
        });
        let_go();
        finish_response(stream, raw)
    }

    /// The error object of the event that ends a stream with an error.
    fn error_event(resp: &Resp) -> Value {
        let text = resp.text();
        assert!(text.ends_with("data: [DONE]\n\n"), "{:?}", text);
        let event = text
            .rsplit("data: ")
            .nth(1)
            .expect("the error event")
            .trim_end();
        serde_json::from_str(event).unwrap()
    }

    /// The same two failures after the SSE stream has started: the status is
    /// already 200, so the OpenAI error event carries the code.
    #[test]
    fn answer_too_large_and_user_stop_after_the_stream_started_are_error_events() {
        for (error, code) in [
            (
                BackendError::TooLarge("ответ 2000000 байт".into()),
                "answer_too_large",
            ),
            (BackendError::Cancelled, "cancelled_by_user"),
        ] {
            let fake = FakeBackend::new();
            fake.close_gate();
            fake.push_reply(Err(error));
            let gw = start_with(&fake, |c| {
                c.keepalive_ms = 50;
                c.first_byte_wait_ms = 30;
            });
            let resp = chat_failing_after_the_stream_started(&fake, gw.port());
            assert_eq!(resp.status, 200);
            let text = resp.text();
            // The role chunk and keepalives came first: the failure arrived
            // after first_byte_wait_ms, as a real turn's always does.
            assert!(text.starts_with("data: {"), "{:?}", text);
            assert!(text.ends_with("data: [DONE]\n\n"), "{:?}", text);
            let event = text
                .rsplit("data: ")
                .nth(1)
                .expect("the error event")
                .trim_end();
            let v: Value = serde_json::from_str(event).unwrap();
            assert_eq!(v["error"]["code"], code, "{:?}", text);
            // OpenCode 2.1.4 reads `error.type`; an unknown one is retried.
            assert_eq!(v["error"]["type"], "invalid_request_error");
            assert!(!text.contains("finish_reason\":\"stop"), "{:?}", text);
            gw.shutdown();
        }
    }

    #[test]
    fn tab_reasons_have_a_neutral_prefix() {
        // The sign-in refusal of an own window comes as a tab reason; it must
        // not read as a lost connection. Still 503, with no retry: it waits
        // on a person.
        let (status, code, text) = describe(&BackendError::Unavailable(
            "ChatGPT в окне cgpt-bridge не выполнил вход — войдите в этом окне".into(),
        ));
        assert_eq!((status, code), (503, "extension_unavailable"));
        assert!(!text.contains("Нет связи"), "{}", text);
        assert!(text.starts_with("cgpt-bridge: "), "{}", text);
        assert!(text.contains("не выполнил вход"), "{}", text);
    }

    /// Every failure of an ask: its status and code, and whether clients are
    /// told they may send it again (Ruling 43: only when the message
    /// certainly did not reach ChatGPT and nothing waits on a person). A
    /// failure not to be resent carries `x-should-retry: false` (OpenCode
    /// 2.1.4 and the OpenAI SDK obey it on any status) and the type
    /// `invalid_request_error` (what OpenCode 2.1.4 reads in a streamed error);
    /// one that may be resent keeps `cgpt_gateway` and no header.
    fn ask_failures() -> Vec<(BackendError, u16, &'static str, bool)> {
        vec![
            // A host restarting: nothing was sent.
            (
                BackendError::NotConnected("хост не запущен".into()),
                503,
                "extension_unavailable",
                true,
            ),
            // The host's socket refuses this user: nothing was sent, but only
            // a person can fix it.
            (
                BackendError::NoAccess("нет прав на сокет".into()),
                503,
                "extension_unavailable",
                false,
            ),
            // The turn did not come, or too little budget was left for the
            // first part: nothing was sent.
            (BackendError::QueueTimeout, 504, "timeout", true),
            // The own tab busy with an earlier answer or ask: this ask typed
            // nothing (after an earlier part of the request it is `Dom`).
            (
                BackendError::TabBusy("вкладка занята".into()),
                503,
                "tab_busy",
                true,
            ),
            // No tab, no sign-in, or the host gone after the ask was handed on.
            (
                BackendError::Unavailable("нет вкладки".into()),
                503,
                "extension_unavailable",
                false,
            ),
            (BackendError::Timeout, 504, "timeout", false),
            (
                BackendError::Dom("нет поля ввода".into()),
                502,
                "dom_failure",
                false,
            ),
            (BackendError::Outdated, 503, "extension_unavailable", false),
            (
                BackendError::TooLarge("ответ 2000000 байт".into()),
                422,
                "answer_too_large",
                false,
            ),
            (BackendError::Cancelled, 422, "cancelled_by_user", false),
            (
                BackendError::Other("что-то".into()),
                500,
                "internal_error",
                false,
            ),
            // Last: it starts a pause that refuses every later request.
            (BackendError::RateLimited, 429, "rate_limited", false),
        ]
    }

    fn assert_retry_advice(v: &Value, should_retry: Option<&str>, retried: bool, what: &str) {
        if retried {
            assert_eq!(should_retry, None, "{}", what);
            assert_eq!(v["error"]["type"], "cgpt_gateway", "{}", what);
        } else {
            assert_eq!(should_retry, Some("false"), "{}", what);
            assert_eq!(v["error"]["type"], "invalid_request_error", "{}", what);
        }
    }

    #[test]
    fn backend_errors_map_to_status() {
        for stream in [false, true] {
            let fake = FakeBackend::new();
            // A long first-byte window: the HTTP error must win even on a loaded machine.
            let gw = start_with(&fake, |c| c.first_byte_wait_ms = 10_000);
            for (error, status, code, retried) in ask_failures() {
                fake.push_reply(Err(error));
                let resp = chat(gw.port(), "chatgpt-web", "hi", stream);
                let what = format!("stream={} code={}", stream, code);
                assert_eq!(resp.status, status, "{}", what);
                let v = resp.json();
                assert_eq!(v["error"]["code"], code, "{}", what);
                assert_retry_advice(&v, resp.header("x-should-retry"), retried, &what);
                assert!(!v["error"]["message"].as_str().unwrap().is_empty());
            }
            gw.shutdown();
        }
        let (_, _, text) = describe(&BackendError::Outdated);
        assert_eq!(
            text,
            "Расширение cgpt-bridge устарело: перезагрузите его из extension/dist"
        );
        let (_, _, text) = describe(&BackendError::RateLimited);
        assert!(text.contains("ограничил частоту"));
        let until_ms = 1_800_000_000_000;
        let (status, code, text) = describe(&BackendError::CoolingDown { until_ms });
        assert_eq!((status, code), (429, "rate_limited"));
        assert_eq!(
            text,
            format!(
                "ChatGPT ограничил частоту запросов, повторите после {}",
                cooldown::clock(until_ms)
            )
        );
    }

    /// The same failures after the SSE stream has started: no header can
    /// change any more, so the error event's type carries the advice. A code
    /// OpenCode 2.1.4 retries whatever the type has another name in the
    /// stream (spec 6.1.1); the HTTP answers above keep the old one.
    ///
    /// OpenCode sorts a streamed error by its code first: any code that
    /// contains `unavailable` or `exhausted`, or is one of its server codes
    /// (`internal_error`, …), is a provider failure it retries, before it
    /// looks at `invalid_request_error`. So the stream name of
    /// `extension_unavailable` must not contain "unavailable" (hence
    /// `bridge_offline`, not `bridge_unavailable`), and neither stream name
    /// may contain "rate limit" or a server phrase (provider-error.ts in
    /// OpenCode v2.1.4, `classifyProviderFailure`).
    #[test]
    fn backend_errors_after_the_stream_started_carry_the_same_advice() {
        for (error, _, code, retried) in ask_failures() {
            let code = match code {
                "extension_unavailable" => "bridge_offline",
                "internal_error" => "bridge_internal",
                other => other,
            };
            let fake = FakeBackend::new();
            fake.close_gate();
            fake.push_reply(Err(error));
            let gw = start_with(&fake, |c| {
                c.keepalive_ms = 50;
                c.first_byte_wait_ms = 30;
            });
            let resp = chat_failing_after_the_stream_started(&fake, gw.port());
            assert_eq!(resp.status, 200, "{}", code);
            let text = resp.text();
            assert!(text.ends_with("data: [DONE]\n\n"), "{:?}", text);
            let event = text
                .rsplit("data: ")
                .nth(1)
                .expect("the error event")
                .trim_end();
            let v: Value = serde_json::from_str(event).unwrap();
            assert_eq!(v["error"]["code"], code, "{:?}", text);
            let kind = if retried {
                "cgpt_gateway"
            } else {
                "invalid_request_error"
            };
            assert_eq!(v["error"]["type"], kind, "{:?}", text);
            assert!(!v["error"]["message"].as_str().unwrap().is_empty());
            gw.shutdown();
        }
    }

    /// A request whose turn did not come within its budget was never sent:
    /// clients may send it again.
    #[test]
    fn a_turn_that_never_came_may_be_retried() {
        let fake = FakeBackend::new();
        fake.close_gate();
        fake.push_reply(Ok(("slow".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.turn_timeout_ms = 250;
            c.min_ask_budget_ms = 0;
        });
        let port = gw.port();
        let first = thread::spawn(move || chat(port, "chatgpt-web", "A", false));
        assert!(fake.wait_for_asks(1), "A started");
        let second = chat(port, "chatgpt-web", "B", false);
        fake.open_gate();
        assert_eq!(second.status, 504);
        let v = second.json();
        assert_eq!(v["error"]["code"], "timeout");
        assert_retry_advice(&v, second.header("x-should-retry"), true, "queue");
        assert_eq!(first.join().unwrap().status, 200);
        assert_eq!(asked(&fake), ["A"], "B never reached ChatGPT");
        gw.shutdown();
    }

    /// The same request offering the `read` tool.
    fn chat_body_with_tools(user: &str, stream: bool) -> String {
        let mut v: Value = serde_json::from_str(&chat_body("chatgpt-web", user, stream)).unwrap();
        v["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                },
            },
        }]);
        v.to_string()
    }

    /// The host gone at a later ask of a request (the second part of a long
    /// message, or the repair of a broken answer) after its first was sent:
    /// the chat has the message, so it is not to be resent, neither before
    /// the stream nor in it. At the first ask the same failure may be resent
    /// (`ask_failures`).
    #[test]
    fn host_gone_after_the_request_sent_a_part_is_not_retried() {
        let two_parts = "task line one\n".repeat(12_000);
        let cases = [
            ("second part", two_parts.as_str(), "ok"),
            ("repair", "прочитай a.txt", "```tool_calls\n{не json\n```"),
        ];
        for (what, user, first) in cases {
            let body = |stream| chat_body_with_tools(user, stream);
            for stream in [false, true] {
                let fake = FakeBackend::new();
                fake.push_reply(Ok((first.into(), Some(CHAT.into()))));
                fake.push_reply(Err(BackendError::NotConnected("хост не запущен".into())));
                let gw = start_with(&fake, |c| {
                    c.keepalive_ms = 50;
                    c.first_byte_wait_ms = 30;
                });
                let what = format!("{} stream={}", what, stream);
                let (v, should_retry) = if stream {
                    // The first ask is held until the stream has started.
                    fake.close_gate();
                    let resp = failing_after_the_stream_started(gw.port(), &body(true), || {
                        fake.open_gate()
                    });
                    assert_eq!(resp.status, 200, "{}", what);
                    (error_event(&resp), None)
                } else {
                    let resp = call(
                        gw.port(),
                        "POST",
                        "/v1/chat/completions",
                        Some(TOKEN),
                        Some(&body(false)),
                    );
                    assert_eq!(resp.status, 503, "{}: {}", what, resp.text());
                    let should_retry = resp.header("x-should-retry").map(str::to_string);
                    (resp.json(), should_retry)
                };
                // The stream's name for `extension_unavailable`.
                let code = if stream {
                    "bridge_offline"
                } else {
                    "extension_unavailable"
                };
                assert_eq!(v["error"]["code"], code, "{}", what);
                if stream {
                    assert_eq!(v["error"]["type"], "invalid_request_error", "{}", what);
                } else {
                    assert_retry_advice(&v, should_retry.as_deref(), false, &what);
                }
                assert_eq!(fake.sent_texts().len(), 2, "{}", what);
                gw.shutdown();
            }
        }
    }

    /// A streamed request whose turn did not come within its budget, after
    /// its stream had started: the event says it may be sent again.
    #[test]
    fn a_turn_that_never_came_after_the_stream_started_may_be_retried() {
        let fake = FakeBackend::new();
        fake.close_gate();
        fake.push_reply(Ok(("slow".into(), Some(CHAT.into()))));
        // B's stream starts after 1 ms; its wait in line runs out after 1 s,
        // while A still holds the turn (until B's stream has ended).
        let gw = start_with(&fake, |c| {
            c.turn_timeout_ms = 1_000;
            c.min_ask_budget_ms = 0;
            c.first_byte_wait_ms = 1;
            c.keepalive_ms = 50;
        });
        let port = gw.port();
        let first = thread::spawn(move || chat(port, "chatgpt-web", "A", false));
        assert!(fake.wait_for_asks(1), "A started");
        let second = chat(port, "chatgpt-web", "B", true);
        fake.open_gate();
        assert_eq!(second.status, 200, "{}", second.text());
        let v = error_event(&second);
        assert_eq!(v["error"]["code"], "timeout");
        assert_eq!(v["error"]["type"], "cgpt_gateway");
        assert!(
            v["error"]["message"].as_str().unwrap().contains("очередь"),
            "{}",
            v
        );
        assert_eq!(first.join().unwrap().status, 200);
        assert_eq!(asked(&fake), ["A"], "B never reached ChatGPT");
        gw.shutdown();
    }

    /// A turn that panicked after its stream had started: the error event
    /// says it is not to be resent (it may have sent its message).
    #[test]
    fn a_turn_crashed_after_the_stream_started_is_not_retried() {
        struct PanicWhenLetGo(Mutex<mpsc::Receiver<()>>);
        impl ChatBackend for PanicWhenLetGo {
            fn ask(
                &self,
                _: &str,
                _: &ChatTarget,
                _: u64,
            ) -> Result<(String, Option<String>), BackendError> {
                let _ = self.0.lock().unwrap().recv();
                panic!("ask blew up")
            }
            fn stop(&self) -> Result<(), BackendError> {
                Ok(())
            }
            fn status(&self) -> Result<bool, BackendError> {
                Ok(true)
            }
        }
        let (go, wait) = mpsc::channel();
        let mut cfg = Config::with_token(TOKEN.to_string());
        cfg.port = 0;
        cfg.first_byte_wait_ms = 30;
        cfg.keepalive_ms = 50;
        let dir = temp_dir("crash-live");
        let engine = Engine::new(
            Sessions::new(dir.clone()),
            Vec::new(),
            Cooldown::new(dir.join("cooldown.json")),
        );
        let gw = start(cfg, Box::new(PanicWhenLetGo(Mutex::new(wait))), engine).unwrap();
        let body = chat_body("chatgpt-web", "hi", true);
        let resp = failing_after_the_stream_started(gw.port(), &body, || go.send(()).unwrap());
        assert_eq!(resp.status, 200);
        let v = error_event(&resp);
        assert_eq!(v["error"]["code"], "bridge_internal");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        gw.shutdown();
    }

    /// A turn that panicked may have sent its message: not to be resent.
    #[test]
    fn a_crashed_turn_is_not_retried() {
        struct PanickingAsk;
        impl ChatBackend for PanickingAsk {
            fn ask(
                &self,
                _: &str,
                _: &ChatTarget,
                _: u64,
            ) -> Result<(String, Option<String>), BackendError> {
                panic!("ask blew up")
            }
            fn stop(&self) -> Result<(), BackendError> {
                Ok(())
            }
            fn status(&self) -> Result<bool, BackendError> {
                Ok(true)
            }
        }
        let mut cfg = Config::with_token(TOKEN.to_string());
        cfg.port = 0;
        cfg.first_byte_wait_ms = 5_000;
        let dir = temp_dir("crash");
        let engine = Engine::new(
            Sessions::new(dir.clone()),
            Vec::new(),
            Cooldown::new(dir.join("cooldown.json")),
        );
        let gw = start(cfg, Box::new(PanickingAsk), engine).unwrap();
        let resp = chat(gw.port(), "chatgpt-web", "hi", true);
        assert_eq!(resp.status, 500, "{}", resp.text());
        let v = resp.json();
        assert_eq!(v["error"]["code"], "internal_error");
        assert_retry_advice(&v, resp.header("x-should-retry"), false, "crash");
        gw.shutdown();
    }

    const FIVE_MIN_MS: u128 = 5 * 60 * 1000;

    /// The `повторите после ЧЧ:ММ` of an error whose pause started between
    /// `before` and now.
    fn assert_pause_ends(v: &Value, before: u128) {
        let message = v["error"]["message"].as_str().unwrap();
        let expected = [before, now_ms()].map(|t| {
            format!(
                "ChatGPT ограничил частоту запросов, повторите после {}",
                cooldown::clock(t + FIVE_MIN_MS)
            )
        });
        assert!(expected.iter().any(|e| e == message), "{}", message);
    }

    /// The `повторите после ЧЧ:ММ` of a 429 whose pause started between
    /// `before` and now.
    fn assert_retry_after(resp: &Resp, before: u128) {
        assert_eq!(resp.status, 429, "{}", resp.text());
        let v = resp.json();
        assert_eq!(v["error"]["code"], "rate_limited");
        assert_pause_ends(&v, before);
        // Seconds until the pause ends, for clients that read the header.
        let secs: u128 = resp
            .header("Retry-After")
            .expect("Retry-After header")
            .parse()
            .unwrap();
        assert!(
            (FIVE_MIN_MS / 1000 - 5..=FIVE_MIN_MS / 1000).contains(&secs),
            "Retry-After {}",
            secs
        );
    }

    /// A rate limit in the turn: a 429 with the end of the pause, and the
    /// pause refuses the next request before the queue. A streamed request
    /// fails after its stream has started (the ask is held until then, so
    /// the outcome does not depend on how fast the turn is): an error event
    /// with the same text.
    #[test]
    fn rate_limited_sets_cooldown_and_returns_429_with_time() {
        for stream in [false, true] {
            let fake = FakeBackend::new();
            fake.push_reply(Err(BackendError::RateLimited));
            let gw = start_with(&fake, |c| {
                c.keepalive_ms = 50;
                c.first_byte_wait_ms = 30;
            });
            let before = now_ms();
            if stream {
                fake.close_gate();
                let resp = chat_failing_after_the_stream_started(&fake, gw.port());
                assert_eq!(resp.status, 200, "{}", resp.text());
                let v = error_event(&resp);
                assert_eq!(v["error"]["code"], "rate_limited");
                assert_eq!(v["error"]["type"], "invalid_request_error");
                assert_pause_ends(&v, before);
            } else {
                let resp = chat(gw.port(), "chatgpt-web", "hi", stream);
                assert_retry_after(&resp, before);
            }
            assert_eq!(fake.stops.load(Ordering::SeqCst), 1, "generation stopped");
            // The pause holds: the next request is refused without an ask.
            let next = chat(gw.port(), "chatgpt-web", "again", stream);
            assert_retry_after(&next, before);
            assert_eq!(asked(&fake), ["hi"], "stream={}", stream);
            gw.shutdown();
        }
    }

    #[test]
    fn active_cooldown_is_a_plain_429_before_the_queue() {
        let fake = FakeBackend::new();
        fake.push_reply(Err(BackendError::RateLimited));
        let gw = start_with(&fake, |c| c.first_byte_wait_ms = 0);
        let before = now_ms();
        assert_eq!(chat(gw.port(), "chatgpt-web", "A", false).status, 429);
        for stream in [false, true] {
            let resp = chat(gw.port(), "chatgpt-web", "B", stream);
            assert_retry_after(&resp, before);
            assert!(resp.header("Content-Type").unwrap().contains("json"));
        }
        assert_eq!(asked(&fake), ["A"], "B never reached ChatGPT");
        // The title model never talks to ChatGPT: no pause for it.
        assert_eq!(chat(gw.port(), "local-title", "B", false).status, 200);
        gw.shutdown();
    }

    #[test]
    fn queued_request_after_a_rate_limit_is_refused_without_asking() {
        let fake = FakeBackend::new();
        // A fails with the rate limit only after B stands in line behind it.
        fake.close_gate();
        fake.push_reply(Err(BackendError::RateLimited));
        fake.push_reply(Ok(("never".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |_| {});
        let port = gw.port();
        let before = now_ms();
        let first = thread::spawn(move || chat(port, "chatgpt-web", "A", false));
        assert!(fake.wait_for_asks(1), "A started");
        let second = thread::spawn(move || chat(port, "chatgpt-web", "B", false));
        wait_for("B to take its place in line", || gw.tickets_taken() == 2);
        fake.open_gate();
        let second = second.join().unwrap();
        assert_retry_after(&second, before);
        assert_eq!(first.join().unwrap().status, 429);
        assert_eq!(asked(&fake), ["A"], "B was never asked");
        gw.shutdown();
    }

    #[test]
    fn error_after_the_stream_started_is_an_error_event_not_assistant_text() {
        let fake = FakeBackend::new();
        // The failure comes only after the stream has started.
        fake.close_gate();
        fake.push_reply(Err(BackendError::Timeout));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 30;
        });
        let resp = chat_failing_after_the_stream_started(&fake, gw.port());
        assert_eq!(resp.status, 200);
        let text = resp.text();
        // Role chunk and keepalives first, then exactly the error event and
        // [DONE]: no content chunk, no finish_reason.
        let tail = "data: {\"error\":{\"code\":\"timeout\",\"message\":\"ChatGPT не ответил за отведённое время\",\"type\":\"invalid_request_error\"}}\n\ndata: [DONE]\n\n";
        assert!(text.ends_with(tail), "{:?}", text);
        let before = &text[..text.len() - tail.len()];
        assert!(before.starts_with("data: {"), "{:?}", before);
        assert_eq!(
            before.matches("data: ").count(),
            1,
            "only the role chunk: {:?}",
            before
        );
        assert!(!text.contains("finish_reason\":\"stop"));
        assert!(!text.contains("⚠️"));
        gw.shutdown();
    }

    #[test]
    fn stop_is_finished_before_the_next_ask_starts() {
        let fake = FakeBackend::new();
        // A's ask ends only when the stop reaches it; the stop then takes a
        // while to finish, long enough for an overlapping ask to show.
        fake.close_gate();
        fake.set_stop_delay(Duration::from_millis(400));
        fake.push_reply(Ok(("A answer".into(), Some(CHAT.into()))));
        fake.push_reply(Ok(("B answer".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 20;
            c.min_ask_budget_ms = 0;
        });
        let port = gw.port();
        // A streams; B queues behind it.
        let body = chat_body("chatgpt-web", "A", true);
        let mut a = send_raw(
            port,
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(&body),
        );
        let mut buf = [0u8; 64];
        assert!(a.read(&mut buf).unwrap() > 0);
        assert!(fake.wait_for_asks(1), "A reached the backend");
        let b = thread::spawn(move || chat(port, "chatgpt-web", "B", false));
        wait_for("B to take its place in line", || gw.tickets_taken() == 2);
        // A's client hangs up: the stop blocks for 400 ms and ends A's ask.
        drop(a);
        // From the stop on the gate may open: it is B's ask that it releases.
        wait_for("the stop to begin", || {
            fake.stops.load(Ordering::SeqCst) == 1
        });
        fake.open_gate();
        assert_eq!(b.join().unwrap().status, 200);
        let log: Vec<String> = fake
            .log()
            .iter()
            .map(|l| match l.strip_prefix("ask:") {
                Some(text) => format!("ask:{}", user_of(text)),
                None => l.clone(),
            })
            .collect();
        assert_eq!(
            log,
            ["ask:A", "stop_begin", "stop_end", "ask:B"],
            "B's ask must not overlap or precede A's stop"
        );
        assert_eq!(fake.stops.load(Ordering::SeqCst), 1);
        assert_eq!(fake.max_concurrent.load(Ordering::SeqCst), 1);
        gw.shutdown();
    }

    #[test]
    fn live_events_arrive_promptly_without_padding() {
        let fake = FakeBackend::new();
        // The answer stays pending for as long as the client reads.
        fake.close_gate();
        fake.push_reply(Ok(("done".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 300;
            c.first_byte_wait_ms = 20;
        });
        let body = chat_body("chatgpt-web", "hi", true);
        let mut s = send_raw(
            gw.port(),
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(&body),
        );
        // Every read must return while the answer is pending (a buffering
        // server would send nothing): the head and role chunk, then each
        // keepalive, as they are written. One read may take at most ten
        // keepalive intervals, so that a paused runner does not fail it; the
        // three keepalives together cannot come sooner than three intervals.
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let sent_at = Instant::now();
        let deadline = sent_at + Duration::from_secs(20);
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        let mut keepalives = 0;
        while keepalives < 3 {
            assert!(Instant::now() < deadline, "no third keepalive: {:?}", seen);
            let n = s.read(&mut buf).expect("event within 3 s");
            assert!(n > 0, "stream ended early: {:?}", seen);
            assert!(n < 1000, "events are small, no padding ({} bytes)", n);
            let part = String::from_utf8_lossy(&buf[..n]).into_owned();
            keepalives += part.matches(": keepalive").count();
            seen.push_str(&part);
        }
        assert!(sent_at.elapsed() >= Duration::from_millis(900));
        assert!(seen.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(seen.contains("Transfer-Encoding: chunked"));
        assert!(seen.contains("\"role\":\"assistant\""));
        assert!(!seen.contains("done"), "the answer is still pending");
        fake.open_gate();
        gw.shutdown();
    }

    #[test]
    fn http_1_0_clients_get_an_unchunked_stream() {
        let fake = FakeBackend::new();
        // The answer is held until the stream has started.
        fake.close_gate();
        fake.push_reply(Ok(("old client".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 20;
        });
        let body = chat_body("chatgpt-web", "hi", true);
        let mut s = TcpStream::connect(("127.0.0.1", gw.port())).unwrap();
        write!(
            s,
            "POST /v1/chat/completions HTTP/1.0\r\nAuthorization: Bearer test-token\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let mut seen = Vec::new();
        read_until(&mut s, &mut seen, |head| {
            head.contains("\"role\":\"assistant\"")
        });
        fake.open_gate();
        s.set_read_timeout(None).unwrap();
        s.read_to_end(&mut seen).unwrap();
        let raw = String::from_utf8(seen).unwrap();
        assert!(raw.starts_with("HTTP/1.0 200 OK"), "{:?}", raw);
        assert!(!raw.contains("chunked"));
        assert!(raw.contains("old client"));
        assert!(raw.ends_with("data: [DONE]\n\n"));
        gw.shutdown();
    }

    #[test]
    fn less_than_the_minimum_budget_after_queueing_is_504_without_asking() {
        let fake = FakeBackend::new();
        // A's ask is held until B has waited about 1.5 s of its 2 s.
        fake.close_gate();
        fake.push_reply(Ok(("slow".into(), Some(CHAT.into()))));
        // B waits ~1.5 s of 2 s: 0.5 s are left, below the default 5 s
        // minimum as clamped to half the timeout (1 s). Should the runner
        // stall for longer, B's wait runs out instead: 504 as well.
        let gw = start_with(&fake, |c| c.turn_timeout_ms = 2_000);
        let port = gw.port();
        let first = thread::spawn(move || chat(port, "chatgpt-web", "A", false));
        assert!(fake.wait_for_asks(1), "A started");
        let second = thread::spawn(move || chat(port, "chatgpt-web", "B", false));
        wait_for("B to take its place in line", || gw.tickets_taken() == 2);
        let_time_pass(Duration::from_millis(1_500));
        fake.open_gate();
        let second = second.join().unwrap();
        assert_eq!(second.status, 504);
        let v = second.json();
        assert_eq!(v["error"]["code"], "timeout");
        // Either way nothing was sent: it may be sent again.
        assert_retry_advice(&v, second.header("x-should-retry"), true, "too little left");
        assert_eq!(first.join().unwrap().status, 200);
        assert_eq!(asked(&fake), ["A"], "B was never asked");
        gw.shutdown();
    }

    #[test]
    fn a_stalled_request_body_does_not_hold_the_turn_line() {
        let fake = FakeBackend::new();
        fake.push_reply(Ok(("ok".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| c.turn_timeout_ms = 2_000);
        let port = gw.port();
        // Headers promise a body that never comes (over 1 KiB: tiny_http
        // reads a smaller body before it hands the request over).
        let mut stalled = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stalled,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: 100000\r\n\r\n{{\"model\":",
            TOKEN
        )
        .unwrap();
        // The handler of the stalled request has begun to read its body,
        // holding no place in line: B is served whenever it comes.
        wait_for("the stalled request's handler to start", || {
            gw.body_reads() == 1
        });
        let resp = chat(port, "chatgpt-web", "B", false);
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(gw.tickets_taken(), 1, "only B took a place");
        drop(stalled);
        gw.shutdown();
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        for auth in [
            "bearer test-token",
            "BEARER test-token",
            "Bearer test-token",
        ] {
            let resp = read_response(send_raw(
                gw.port(),
                "GET",
                "/v1/models",
                &[("Authorization", auth)],
                None,
            ));
            assert_eq!(resp.status, 200, "{}", auth);
        }
        let resp = read_response(send_raw(
            gw.port(),
            "GET",
            "/v1/models",
            &[("Authorization", "bearer wrong")],
            None,
        ));
        assert_eq!(resp.status, 401);
        gw.shutdown();
    }

    // ---- serialization

    #[test]
    fn requests_are_serialized() {
        let fake = FakeBackend::new();
        // The first ask is held until all three requests are in line: were
        // they not serialized, the others would reach the backend meanwhile.
        fake.close_gate();
        for i in 0..3 {
            fake.push_reply(Ok((format!("answer {}", i), Some(CHAT.into()))));
        }
        let gw = start_with(&fake, |_| {});
        let port = gw.port();
        let handles: Vec<_> = (0..3)
            .map(|i| {
                thread::spawn(move || chat(port, "chatgpt-web", &format!("question {}", i), false))
            })
            .collect();
        wait_for("all three to take their places", || gw.tickets_taken() == 3);
        assert!(fake.wait_for_asks(1), "one ask reached the backend");
        fake.open_gate();
        for h in handles {
            assert_eq!(h.join().unwrap().status, 200);
        }
        assert_eq!(fake.sent.lock().unwrap().len(), 3);
        assert_eq!(
            fake.max_concurrent.load(Ordering::SeqCst),
            1,
            "asks overlapped"
        );
        gw.shutdown();
    }

    #[test]
    fn a_second_part_with_less_than_the_minimum_budget_is_504_and_not_sent() {
        let fake = FakeBackend::new();
        // The turn has 60 s, the minimum for a part is 30 s (half). Part 1
        // takes 31 s of the turn's clock (no real wait), so part 2 would start
        // with under 29 s: above zero, under the minimum. Preparing the
        // message runs on the wall clock and has up to 30 s before part 1.
        fake.set_turn_time(Duration::from_secs(31));
        fake.push_reply(Ok(("ok".into(), Some(CHAT.into()))));
        fake.push_reply(Ok(("never".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.turn_timeout_ms = 60_000;
            c.min_ask_budget_ms = 30_000;
        });
        let two_parts = "task line one\n".repeat(12_000);
        let resp = chat(gw.port(), "chatgpt-web", &two_parts, false);
        assert_eq!(resp.status, 504, "{}", resp.text());
        let v = resp.json();
        assert_eq!(v["error"]["code"], "timeout");
        // The first part was sent: not to be resent.
        assert_retry_advice(&v, resp.header("x-should-retry"), false, "after part 1");
        assert_eq!(fake.sent_texts().len(), 1, "the second part was not sent");
        assert_eq!(fake.stops.load(Ordering::SeqCst), 0, "nothing to stop");
        gw.shutdown();
    }

    #[test]
    fn waiting_longer_than_the_turn_timeout_is_504() {
        let fake = FakeBackend::new();
        // A holds the turn until B's wait has run out.
        fake.close_gate();
        fake.push_reply(Ok(("slow".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.turn_timeout_ms = 250;
            c.min_ask_budget_ms = 0;
        });
        let port = gw.port();
        let first = thread::spawn(move || chat(port, "chatgpt-web", "A", false));
        assert!(fake.wait_for_asks(1), "A started");
        let second = chat(port, "chatgpt-web", "B", false);
        fake.open_gate();
        assert_eq!(second.status, 504);
        assert_eq!(second.json()["error"]["code"], "timeout");
        assert_eq!(first.join().unwrap().status, 200);
        assert_eq!(
            fake.sent.lock().unwrap().len(),
            1,
            "B never reached ChatGPT"
        );
        gw.shutdown();
    }

    // ---- UTF-8

    #[test]
    fn utf8_cyrillic_and_emoji_roundtrip() {
        let prompt = "Привет, мир 🎉 — ёж, 日本語, 𝒳";
        // Long enough to cross several 8 KiB chunk boundaries of the body.
        let answer = "Ответ 🦀 — съешь ещё этих мягких французских булок 😀. ".repeat(900);
        for stream in [false, true] {
            let fake = FakeBackend::new();
            fake.push_reply(Ok((answer.clone(), Some(CHAT.into()))));
            let gw = start_with(&fake, |_| {});
            let resp = chat(gw.port(), "chatgpt-web", prompt, stream);
            assert_eq!(resp.status, 200);
            let got = if stream {
                let (events, done) = resp.sse();
                assert!(done);
                assistant_text(&events)
            } else {
                resp.json()["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            // Text answers are trimmed by the reply parser.
            assert_eq!(got, answer.trim_end(), "stream={}", stream);
            assert_eq!(asked(&fake), [prompt], "stream={}", stream);
            gw.shutdown();
        }
    }

    #[test]
    fn utf8_in_a_live_stream_with_keepalives() {
        let answer = "🎉".repeat(5000);
        let fake = FakeBackend::new();
        // The answer is held until the client has seen a keepalive.
        fake.close_gate();
        fake.push_reply(Ok((answer.clone(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 60;
            c.first_byte_wait_ms = 30;
        });
        let body = chat_body("chatgpt-web", "hi", true);
        let mut stream = send_raw(
            gw.port(),
            "POST",
            "/v1/chat/completions",
            &[("Authorization", "Bearer test-token")],
            Some(&body),
        );
        let mut raw = Vec::new();
        read_until(&mut stream, &mut raw, |seen| seen.contains(": keepalive"));
        fake.open_gate();
        let resp = finish_response(stream, raw);
        assert!(resp.text().contains(": keepalive"));
        let (events, done) = resp.sse();
        assert!(done);
        assert_eq!(assistant_text(&events), answer);
        gw.shutdown();
    }

    // ---- sessions over HTTP

    /// A streamed request with a session header, on a fresh connection.
    fn send_session(port: u16, session: &str, body: &str) -> TcpStream {
        send_raw(
            port,
            "POST",
            "/v1/chat/completions",
            &[
                ("Authorization", "Bearer test-token"),
                ("x-opencode-session-id", session),
            ],
            Some(body),
        )
    }

    #[test]
    fn retry_after_client_stop_has_note_and_no_bootstrap() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 20;
        });
        let port = gw.port();
        let url = "https://chatgpt.com/c/s1";
        let first = json!({
            "model": "chatgpt-web",
            "messages": [{ "role": "system", "content": "rules" }, { "role": "user", "content": "привет" }],
        });
        fake.push_reply(Ok(("здравствуй".into(), Some(url.into()))));
        assert_eq!(
            read_response(send_session(port, "ses_http", &first.to_string())).status,
            200
        );

        // The next turn streams; the client presses Stop while ChatGPT writes.
        let mut second = first.clone();
        second["messages"].as_array_mut().unwrap().extend([
            json!({"role": "assistant", "content": "здравствуй"}),
            json!({"role": "user", "content": "расскажи сказку"}),
        ]);
        second["stream"] = json!(true);
        // ChatGPT is still writing: only the stop ends the ask.
        fake.close_gate();
        fake.push_reply(Ok(("Жили-были…".into(), Some(url.into()))));
        let mut stream = send_session(port, "ses_http", &second.to_string());
        let mut buf = [0u8; 64];
        assert!(stream.read(&mut buf).unwrap() > 0);
        // The streamed ask (the second of the test) is under way.
        assert!(fake.wait_for_asks(2), "the ask started");
        drop(stream);
        wait_for("stop after disconnect", || {
            fake.stops.load(Ordering::SeqCst) == 1
        });

        // OpenCode sends the same request again.
        fake.open_gate();
        fake.push_reply(Ok(("Жили-были дед и баба.".into(), Some(url.into()))));
        second["stream"] = json!(false);
        wait_for("the stopped turn to end", || {
            fake.replies.lock().unwrap().len() == 1
        });
        let resp = read_response(send_session(port, "ses_http", &second.to_string()));
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.json()["choices"][0]["message"]["content"],
            "Жили-были дед и баба."
        );
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[2].1, ChatTarget::Conversation { url: url.into() });
        assert!(
            sent[2].0.starts_with(crate::prompt::INTERRUPTED_NOTE),
            "{}",
            sent[2].0
        );
        assert_eq!(sent[2].0.matches("расскажи сказку").count(), 1);
        assert!(!sent[2].0.contains("Правила шлюза"), "no bootstrap again");
        gw.shutdown();
    }

    #[test]
    fn next_request_without_our_answer_has_the_note() {
        // The client got nothing of the answer (it left in its last
        // seconds) and sends a new message on the history it had.
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        let port = gw.port();
        let url = "https://chatgpt.com/c/s2";
        let first = json!({
            "model": "chatgpt-web",
            "messages": [{ "role": "system", "content": "rules" }, { "role": "user", "content": "привет" }],
        });
        fake.push_reply(Ok(("здравствуй".into(), Some(url.into()))));
        assert_eq!(
            read_response(send_session(port, "ses_lost", &first.to_string())).status,
            200
        );

        let mut second = first.clone();
        second["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "user", "content": "ты тут?"}));
        fake.push_reply(Ok(("да".into(), Some(url.into()))));
        assert_eq!(
            read_response(send_session(port, "ses_lost", &second.to_string())).status,
            200
        );
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].1, ChatTarget::Conversation { url: url.into() });
        assert!(
            sent[1].0.starts_with(crate::prompt::INTERRUPTED_NOTE),
            "{}",
            sent[1].0
        );
        assert_eq!(
            sent[1].0.matches(crate::prompt::INTERRUPTED_NOTE).count(),
            1,
            "{}",
            sent[1].0
        );
        assert!(!sent[1].0.contains("Правила шлюза"), "no bootstrap again");

        // With the answer in the history there is no note.
        second["messages"].as_array_mut().unwrap().extend([
            json!({"role": "assistant", "content": "да"}),
            json!({"role": "user", "content": "хорошо"}),
        ]);
        fake.push_reply(Ok(("ок".into(), Some(url.into()))));
        assert_eq!(
            read_response(send_session(port, "ses_lost", &second.to_string())).status,
            200
        );
        let sent = fake.sent.lock().unwrap().clone();
        assert!(!sent[2].0.contains(crate::prompt::INTERRUPTED_NOTE));
        gw.shutdown();
    }

    // ---- connection handling

    #[test]
    fn stop_state_is_done_even_if_stop_panics() {
        struct PanickingStop;
        impl ChatBackend for PanickingStop {
            fn ask(
                &self,
                _: &str,
                _: &ChatTarget,
                _: u64,
            ) -> Result<(String, Option<String>), BackendError> {
                unreachable!()
            }
            fn stop(&self) -> Result<(), BackendError> {
                panic!("stop blew up")
            }
            fn status(&self) -> Result<bool, BackendError> {
                unreachable!()
            }
        }
        let cancel = Arc::new(Cancel::default());
        // The test ends the ask itself, below.
        std::mem::forget(cancel.begin_ask().expect("ask begins"));
        let handler = {
            let cancel = cancel.clone();
            thread::spawn(move || cancel.cancel(&PanickingStop))
        };
        assert!(handler.join().is_err(), "stop panicked");
        // The worker is not left waiting for a stop that will never finish.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(cancel.end_ask()).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(true));
    }

    /// Read exactly one response from a kept-alive connection.
    fn read_one(reader: &mut std::io::BufReader<TcpStream>) -> Resp {
        use std::io::BufRead;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            reader.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let mut lines = head.lines();
        let status = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let find = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        let mut body = Vec::new();
        if find("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let size = usize::from_str_radix(line.trim(), 16).unwrap();
                let mut chunk = vec![0u8; size + 2];
                reader.read_exact(&mut chunk).unwrap();
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        } else {
            let len: usize = find("content-length")
                .expect("a framed body")
                .parse()
                .unwrap();
            body.resize(len, 0);
            reader.read_exact(&mut body).unwrap();
        }
        Resp {
            status,
            headers,
            body,
        }
    }

    #[test]
    fn keep_alive_connection_serves_two_streams_and_a_get() {
        let fake = FakeBackend::new();
        // Each answer is held until the stream shows a keepalive.
        fake.close_gate();
        fake.push_reply(Ok(("первый".into(), Some(CHAT.into()))));
        fake.push_reply(Ok(("второй".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 20;
        });
        let stream = TcpStream::connect(("127.0.0.1", gw.port())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let peeker = stream.try_clone().unwrap();
        let mut reader = std::io::BufReader::new(stream);
        for (user, answer) in [("one", "первый"), ("two", "второй")] {
            let body = chat_body("chatgpt-web", user, true);
            write!(
                writer,
                "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            // Peek, so that the response is still there to be read whole.
            wait_for("a keepalive on the stream", || {
                let mut seen = [0u8; 4096];
                let n = peeker.peek(&mut seen).unwrap();
                String::from_utf8_lossy(&seen[..n]).contains(": keepalive")
            });
            fake.open_gate();
            let resp = read_one(&mut reader);
            fake.close_gate();
            assert_eq!(resp.status, 200);
            assert!(resp.text().contains(": keepalive"), "a live stream");
            let (events, done) = resp.sse();
            assert!(done);
            assert_eq!(assistant_text(&events), answer);
        }
        write!(
            writer,
            "GET /v1/models HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-token\r\n\r\n"
        )
        .unwrap();
        let resp = read_one(&mut reader);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.json()["object"], "list");
        gw.shutdown();
    }

    #[test]
    fn http_1_0_keep_alive_client_gets_a_complete_stream() {
        let fake = FakeBackend::new();
        fake.set_delay(Duration::from_millis(200));
        fake.push_reply(Ok(("old client".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| {
            c.keepalive_ms = 50;
            c.first_byte_wait_ms = 20;
        });
        let body = chat_body("chatgpt-web", "hi", true);
        let stream = TcpStream::connect(("127.0.0.1", gw.port())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        write!(
            writer,
            "POST /v1/chat/completions HTTP/1.0\r\nConnection: keep-alive\r\nAuthorization: Bearer test-token\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        // The client can tell where the answer ends without waiting for the
        // server to close the connection.
        let resp = read_one(&mut std::io::BufReader::new(stream));
        assert_eq!(resp.status, 200);
        let (events, done) = resp.sse();
        assert!(done);
        assert_eq!(assistant_text(&events), "old client");
        gw.shutdown();
    }

    #[test]
    fn min_ask_budget_is_at_most_half_the_turn_timeout() {
        let fake = FakeBackend::new();
        fake.push_reply(Ok(("ok".into(), Some(CHAT.into()))));
        // The default 5 s minimum would refuse every turn of a 2 s timeout.
        let gw = start_with(&fake, |c| c.turn_timeout_ms = 2_000);
        assert_eq!(chat(gw.port(), "chatgpt-web", "hi", false).status, 200);
        gw.shutdown();
    }

    #[test]
    fn a_panicking_turn_still_ends_its_ask() {
        struct PanickingAsk(Arc<AtomicU32>);
        impl ChatBackend for PanickingAsk {
            fn ask(
                &self,
                _: &str,
                _: &ChatTarget,
                _: u64,
            ) -> Result<(String, Option<String>), BackendError> {
                panic!("ask blew up")
            }
            fn stop(&self) -> Result<(), BackendError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn status(&self) -> Result<bool, BackendError> {
                Ok(true)
            }
        }
        let stops = Arc::new(AtomicU32::new(0));
        let dir = temp_dir("panic");
        let sh = Arc::new(Shared {
            cfg: Config::with_token(TOKEN.to_string()),
            backend: Box::new(PanickingAsk(stops.clone())),
            cooldown: Cooldown::new(dir.join("cooldown.json")),
            gate: TurnGate::new(Engine::new(
                Sessions::new(dir.clone()),
                Vec::new(),
                Cooldown::new(dir.join("cooldown.json")),
            )),
            hangups: std::sync::atomic::AtomicUsize::new(0),
            body_reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let req: ChatRequest =
            serde_json::from_str(&chat_body("chatgpt-web", "hi", false)).unwrap();
        let cancel = Cancel::default();
        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_turn(&sh, Place::take(&sh), &req, &[], &cancel)
        }));
        assert!(got.is_err(), "the ask panicked");
        assert!(!lock(&cancel.state).in_ask, "the ask was ended");
        // A client leaving now must not stop a generation that is not this
        // request's.
        cancel.cancel(&*sh.backend);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        assert!(
            sh.gate.acquire(Duration::from_millis(100)).is_some(),
            "the gate is free"
        );
    }

    #[test]
    fn concurrent_sessions_are_serialized_and_keep_their_chats() {
        let fake = FakeBackend::new();
        let gw = start_with(&fake, |_| {});
        let port = gw.port();
        let first = |s: &str| {
            json!({
                "model": "chatgpt-web",
                "messages": [{ "role": "user", "content": format!("задача {}", s) }],
            })
        };
        // The first ask of a round is held until both requests are in line:
        // were they not serialized, the second would reach the backend too.
        let both = |bodies: [(String, Value); 2], tickets: u64| {
            fake.close_gate();
            let handles: Vec<_> = bodies
                .into_iter()
                .map(|(session, body)| {
                    thread::spawn(move || {
                        let resp = read_response(send_session(port, &session, &body.to_string()));
                        assert_eq!(resp.status, 200);
                        resp.json()["choices"][0]["message"]["content"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                })
                .collect();
            wait_for("both requests to take their places", || {
                gw.tickets_taken() == tickets
            });
            fake.open_gate();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<String>>()
        };
        // Round 1: both new, at the same time. Whoever comes first gets the
        // first scripted reply and its chat.
        fake.push_reply(Ok((
            "ответ 1".into(),
            Some("https://chatgpt.com/c/one".into()),
        )));
        fake.push_reply(Ok((
            "ответ 2".into(),
            Some("https://chatgpt.com/c/two".into()),
        )));
        let answers = both(
            [("ses_A".into(), first("A")), ("ses_B".into(), first("B"))],
            2,
        );
        let chat_of = |s: &str| {
            let sent = fake.sent.lock().unwrap().clone();
            let i = sent
                .iter()
                .position(|(t, _)| t.contains(&format!("задача {}", s)))
                .unwrap();
            ["https://chatgpt.com/c/one", "https://chatgpt.com/c/two"][i].to_string()
        };
        let (chat_a, chat_b) = (chat_of("A"), chat_of("B"));
        assert_ne!(chat_a, chat_b);

        // Round 2: both continue, at the same time.
        fake.push_reply(Ok(("ок".into(), Some(IN_TARGET_CHAT.into()))));
        fake.push_reply(Ok(("ок".into(), Some(IN_TARGET_CHAT.into()))));
        let next = |s: &str, answer: &str| {
            let mut body = first(s);
            body["messages"].as_array_mut().unwrap().extend([
                json!({"role": "assistant", "content": answer}),
                json!({"role": "user", "content": format!("ещё {}", s)}),
            ]);
            body
        };
        both(
            [
                ("ses_A".into(), next("A", &answers[0])),
                ("ses_B".into(), next("B", &answers[1])),
            ],
            4,
        );
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 4);
        for (text, target) in &sent[2..] {
            let (mine, other, chat) = if text.contains("ещё A") {
                ("A", "B", &chat_a)
            } else {
                ("B", "A", &chat_b)
            };
            assert_eq!(
                target,
                &ChatTarget::Conversation { url: chat.clone() },
                "{}",
                mine
            );
            assert!(!text.contains(&format!("ещё {}", other)));
            assert!(!text.contains("Правила шлюза"), "no new chat for {}", mine);
        }
        assert_eq!(
            fake.max_concurrent.load(Ordering::SeqCst),
            1,
            "asks overlapped"
        );
        gw.shutdown();
    }

    #[test]
    fn content_may_be_a_list_of_parts() {
        let fake = FakeBackend::new();
        fake.push_reply(Ok(("ok".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |_| {});
        let body = json!({
            "model": "chatgpt-web",
            "messages": [{ "role": "user", "content": [{"type":"text","text":"часть один"},{"type":"text","text":"часть два"}] }],
        })
        .to_string();
        let resp = call(
            gw.port(),
            "POST",
            "/v1/chat/completions",
            Some(TOKEN),
            Some(&body),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(asked(&fake), ["часть один\nчасть два"]);
        gw.shutdown();
    }

    #[test]
    fn turn_gate_serves_waiters_first_come_first_served() {
        let gate = Arc::new(TurnGate::new(()));
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = gate.acquire(Duration::from_secs(1)).unwrap();
        let mut waiters = Vec::new();
        for (i, name) in ["A", "B", "C"].into_iter().enumerate() {
            let (line, order) = (gate.clone(), order.clone());
            waiters.push(thread::spawn(move || {
                let _turn = line.acquire(Duration::from_secs(5)).unwrap();
                lock(&order).push(name);
            }));
            // Each waiter is in line before the next one comes (the holder
            // took the first ticket).
            wait_for("the waiter to take its ticket", || {
                gate.tickets_taken() == i as u64 + 2
            });
        }
        // The holder lets go and asks again at once: it is behind A, B and C.
        drop(held);
        let again = gate.acquire(Duration::from_secs(5)).unwrap();
        lock(&order).push("D");
        drop(again);
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(*lock(&order), ["A", "B", "C", "D"]);
    }

    #[test]
    fn turn_gate_serves_tickets_in_the_order_they_were_taken() {
        // The ticket is taken when the request arrives; the handler may come
        // to wait later than one that arrived after it.
        let gate = Arc::new(TurnGate::new(()));
        let order = Arc::new(Mutex::new(Vec::new()));
        let a = gate.ticket();
        let b = gate.ticket();
        let second = {
            let (gate, order) = (gate.clone(), order.clone());
            thread::spawn(move || {
                let _turn = gate.wait(b, Duration::from_secs(5)).unwrap();
                lock(&order).push("B");
            })
        };
        // B has found the gate free but A first in line, and sleeps.
        wait_for("B to wait for its turn", || gate.parked() >= 1);
        assert!(lock(&order).is_empty(), "B went ahead of A");
        let turn = gate.wait(a, Duration::from_secs(1)).unwrap();
        lock(&order).push("A");
        drop(turn);
        second.join().unwrap();
        assert_eq!(*lock(&order), ["A", "B"]);
    }

    #[test]
    fn turn_gate_ticket_left_unused_leaves_the_line() {
        let gate = TurnGate::new(());
        let a = gate.ticket();
        let b = gate.ticket();
        gate.leave(a);
        assert!(gate.wait(b, Duration::from_millis(100)).is_some());
    }

    #[test]
    fn turn_gate_waiter_that_gives_up_leaves_the_line() {
        let gate = Arc::new(TurnGate::new(()));
        let held = gate.acquire(Duration::from_secs(1)).unwrap();
        assert!(gate.acquire(Duration::from_millis(50)).is_none());
        let next = {
            let gate = gate.clone();
            thread::spawn(move || gate.acquire(Duration::from_secs(5)).is_some())
        };
        // Held's ticket was taken first, the leaver's second, this one's third.
        wait_for("the next one to take its ticket", || {
            gate.tickets_taken() == 3
        });
        drop(held);
        assert!(next.join().unwrap(), "the one behind the leaver is served");
        assert!(gate.acquire(Duration::from_millis(100)).is_some());
    }

    #[test]
    fn a_huge_turn_timeout_does_not_panic() {
        let fake = FakeBackend::new();
        fake.push_reply(Ok(("ok".into(), Some(CHAT.into()))));
        let gw = start_with(&fake, |c| c.turn_timeout_ms = u64::MAX);
        assert_eq!(chat(gw.port(), "chatgpt-web", "hi", false).status, 200);
        gw.shutdown();
    }
}
