//! Core of cgpt-bridge: command denylist, output redaction and the ChatGPT
//! project a new chat goes into.
//!
//! Shared by the `cgpt` CLI and the gateway.

pub mod denylist;
pub mod limits;
pub mod project;
pub mod redact;
pub mod time;
