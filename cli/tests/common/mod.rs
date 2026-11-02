//! Shared by the mock-host tests (`agent_chat`, `exit_codes`,
//! `input_redaction`).

use std::thread;
use std::time::{Duration, Instant};

use cgpt_bridge_protocol::connect;

/// A mock host thread plus the endpoint it listens on. `join` cannot hang:
/// if the host is still waiting in `accept` after the CLI process is gone
/// (the CLI exited without connecting), a throwaway connection wakes it, its
/// `read_frame` fails and `join` returns `Err`, so the test fails.
pub struct Host<T> {
    handle: thread::JoinHandle<T>,
    endpoint: String,
}

impl<T> Host<T> {
    pub fn new(endpoint: String, handle: thread::JoinHandle<T>) -> Self {
        Host { handle, endpoint }
    }

    /// Call after the CLI process has exited.
    pub fn join(self) -> thread::Result<T> {
        // A host that served the CLI is finishing; give it a moment first.
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !self.handle.is_finished() {
            let _ = connect(&self.endpoint, 200);
        }
        self.handle.join()
    }
}
