//! How long each local process keeps waiting past an ask's `timeout_ms`.
//!
//! The extension bounds its own work by `timeout_ms` and answers with a
//! `timeout` error. The host waits a little longer, so it never beats the
//! extension to that error; the host's clients (the CLI, the gateway) wait
//! longer still, so the host's reply (a partial answer at the deadline, or its
//! own timeout error) reaches them before they give up.

/// How long the host waits for the extension past an ask's `timeout_ms`.
pub const HOST_TIMEOUT_GRACE_MS: u64 = 5_000;

/// How long a client of the host (the CLI, the gateway) waits for the host's
/// reply past an ask's `timeout_ms`.
pub const CLIENT_TIMEOUT_GRACE_MS: u64 = 10_000;

// A client that gave up before the host would never see the host's reply.
const _: () = assert!(CLIENT_TIMEOUT_GRACE_MS > HOST_TIMEOUT_GRACE_MS);
