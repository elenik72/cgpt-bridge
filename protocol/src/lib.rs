//! Shared transport-level types for the cgpt-bridge project.
//!
//! Two callers use this crate today:
//!
//! - `cgpt-bridge-host` (the Chrome Native Messaging host) — speaks framing
//!   on stdin/stdout with Chrome and (in M5 phase 2) on a Unix domain socket
//!   with the CLI.
//! - `cgpt` (the user-facing CLI) — speaks framing on a Unix domain socket
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
pub mod framing;
pub mod message;
pub mod socket_path;

pub use framing::{read_frame, write_frame, ReadFrameError, MAX_FRAME_BYTES};
pub use message::{
    dispatch_basic, error_response, response_id, AskRequest, BridgeRequest, BridgeResponse,
    ErrorCode, PingRequest, HOST_VERSION,
};
pub use socket_path::default_socket_path;
