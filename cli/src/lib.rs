//! Public surface of the `cgpt` CLI, exposed primarily so integration tests
//! can drive `transport::ask_once` without spawning a child process.
//!
//! The `main` binary lives in `src/main.rs` and is the only intended public
//! entry point for users.

pub mod agent;
pub mod args;
pub mod denylist;
pub mod plan;
pub mod redact;
pub mod runner;
pub mod spinner;
pub mod transport;
