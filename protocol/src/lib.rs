//! Shared transport-level types for the cgpt-bridge project.
//!
//! Two callers use this crate today:
//!
//! - `cgpt-bridge-host` (the Chrome Native Messaging host) — speaks framing
//!   on stdin/stdout with Chrome and on a local socket (Unix domain socket,
//!   Windows named pipe) with the CLI.
//! - `cgpt` (the user-facing CLI) — speaks framing on that local socket
//!   with the host.
//!
//! Framing is identical on both transports: 4-byte little-endian length prefix
//! followed by exactly that many bytes of UTF-8 JSON. This matches Chrome's
//! Native Messaging wire format, so the host's stdio loop and its CLI server
//! loop can share one implementation.
//!
//! The assistant-side protocol (`cgpt-agent-response-v1`, etc.) defined in
//! `docs/protocol.md` is a *higher* layer than what lives here and is parsed
//! inside the CLI from assistant text. This crate only models the
//! transport-level envelopes between local processes.

pub mod agent;
pub mod chat_url;
pub mod fence;
pub mod framing;
pub mod grace;
pub mod message;
pub mod local_socket;
pub mod request_id;

pub use framing::{read_frame, write_frame, ReadFrameError, MAX_FRAME_BYTES};
pub use grace::{CLIENT_TIMEOUT_GRACE_MS, HOST_TIMEOUT_GRACE_MS};
pub use message::{
    check_project_name, dispatch_basic, error_response, response_id, AskRequest, BridgeRequest,
    BridgeResponse, ChatTarget, ErrorCode, PingRequest, StatusRequest, StopAskRequest, StopRequest, BUILD,
    HOST_VERSION, MAX_PROJECT_NAME_CHARS,
};
pub use request_id::new_request_id;
#[cfg(unix)]
pub use local_socket::default_socket_path;
pub use local_socket::{
    accept, connect, default_endpoint, flush_to_peer, listen, release, round_trip, LocalListener,
    LocalStream,
};
