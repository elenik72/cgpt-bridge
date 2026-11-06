//! Which chat requests may run their turn now (several OpenCode sessions at
//! once, T7 design rules 1-3).
//!
//! - Turns of one session (`Sessions::key_for`) run strictly one at a time,
//!   in the order the requests came.
//! - Turns of different sessions run at once, up to the limit: the smaller of
//!   `max_parallel` (config) and the own tabs the extension reported in its
//!   last `status` (`own_tabs`). Until an extension has reported them, or
//!   when it (or the host) is too old to, the limit is 1: one turn at a time,
//!   exactly as with the one-seat gate before.
//! - A free seat goes to the earliest waiting request whose session has no
//!   turn running (one line in arrival order, no overtaking inside a
//!   session).
//!
//! A request takes its ticket once it has fully arrived (`ticket`) and waits
//! on it later (`wait`), on whatever thread runs the turn. Waiting is bounded,
//! so a request stuck behind long turns can give up and leave the line.

use std::collections::{HashSet, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::config::{MAX_MAX_PARALLEL, MIN_MAX_PARALLEL};
use crate::log::log;

pub struct Lanes {
    state: Mutex<State>,
    changed: Condvar,
    /// `max_parallel` of the config, within 1..=8.
    max_parallel: usize,
    /// Times a waiter went to sleep, for tests to observe.
    #[cfg(test)]
    parked: std::sync::atomic::AtomicUsize,
}

struct State {
    /// The waiting requests, first come first: ticket and session key.
    line: VecDeque<(u64, String)>,
    /// The sessions with a turn running (each at most once).
    running: HashSet<String>,
    next_ticket: u64,
    /// See [`Lanes::set_own_tabs`].
    own_tabs: Option<u32>,
}

/// A running turn's seat; gives it back when dropped.
pub struct Seat<'a> {
    lanes: &'a Lanes,
    key: String,
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Lanes {
    pub fn new(max_parallel: u32) -> Lanes {
        Lanes {
            state: Mutex::new(State {
                line: VecDeque::new(),
                running: HashSet::new(),
                next_ticket: 0,
                own_tabs: None,
            }),
            changed: Condvar::new(),
            max_parallel: max_parallel.clamp(MIN_MAX_PARALLEL, MAX_MAX_PARALLEL) as usize,
            #[cfg(test)]
            parked: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn limit_with(&self, own_tabs: Option<u32>) -> usize {
        match own_tabs {
            None => 1,
            Some(n) => (n as usize).clamp(1, self.max_parallel),
        }
    }

    /// Turns that may run at once now.
    #[cfg(test)]
    pub fn limit(&self) -> usize {
        self.limit_with(lock(&self.state).own_tabs)
    }

    /// The extension's `own_tabs` from the last `status` it answered (`None`:
    /// it sent none). Called after every status probe; a waiter may get a
    /// seat when the limit grows.
    pub fn set_own_tabs(&self, own_tabs: Option<u32>) {
        let mut state = lock(&self.state);
        if state.own_tabs == own_tabs {
            return;
        }
        let before = self.limit_with(state.own_tabs);
        state.own_tabs = own_tabs;
        let after = self.limit_with(own_tabs);
        drop(state);
        if before != after {
            log(format_args!(
                "cgpt-gateway: turns of different sessions at once: {} (max_parallel {}, own tabs {})",
                after,
                self.max_parallel,
                own_tabs.map_or("none".to_string(), |n| n.to_string())
            ));
            self.changed.notify_all();
        }
    }

    /// A place at the end of the line for a turn of session `key`. It must
    /// be waited on or left.
    pub fn ticket(&self, key: String) -> u64 {
        let mut state = lock(&self.state);
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        state.line.push_back((ticket, key));
        ticket
    }

    /// Give up a ticket that will not be waited on.
    pub fn leave(&self, ticket: u64) {
        lock(&self.state).line.retain(|(t, _)| *t != ticket);
        // The next in line may be free to go now.
        self.changed.notify_all();
    }

    /// Wait until `ticket` may run its turn: a seat is free, and it is the
    /// earliest in line whose session has no turn running. `None` after
    /// `timeout`, and the ticket has left the line.
    pub fn wait(&self, ticket: u64, timeout: Duration) -> Option<Seat<'_>> {
        let start = Instant::now();
        let mut state = lock(&self.state);
        loop {
            let limit = self.limit_with(state.own_tabs);
            let next = state
                .line
                .iter()
                .position(|(_, key)| !state.running.contains(key));
            if state.running.len() < limit {
                if let Some(i) = next.filter(|&i| state.line[i].0 == ticket) {
                    let (_, key) = state.line.remove(i).expect("the position is in the line");
                    state.running.insert(key.clone());
                    // Another seat may be free for the next one in line.
                    self.changed.notify_all();
                    return Some(Seat { lanes: self, key });
                }
            }
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                state.line.retain(|(t, _)| *t != ticket);
                // The next in line may be free to go now.
                self.changed.notify_all();
                return None;
            }
            #[cfg(test)]
            self.parked
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Tickets taken so far, whether or not they are still in line.
    #[cfg(test)]
    pub fn tickets_taken(&self) -> u64 {
        lock(&self.state).next_ticket
    }

    /// Times a waiter has found its turn not yet come and gone to sleep.
    #[cfg(test)]
    pub fn parked(&self) -> usize {
        self.parked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Turns running now (each holds a seat until it has ended, its stop
    /// included).
    pub fn running(&self) -> usize {
        lock(&self.state).running.len()
    }

    #[cfg(test)]
    pub fn acquire(&self, key: &str, timeout: Duration) -> Option<Seat<'_>> {
        self.wait(self.ticket(key.to_string()), timeout)
    }
}

impl Drop for Seat<'_> {
    fn drop(&mut self) {
        lock(&self.lanes.state).running.remove(&self.key);
        // Every waiter checks whether its turn has come.
        self.lanes.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    /// Wait for `cond` (a poll; the deadline only turns a hang into a
    /// failure).
    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {}", what);
            thread::sleep(Duration::from_millis(1));
        }
    }

    const LONG: Duration = Duration::from_secs(5);

    #[test]
    fn one_session_is_served_first_come_first_served() {
        let lanes = Arc::new(Lanes::new(3));
        lanes.set_own_tabs(Some(8));
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = lanes.acquire("s", LONG).unwrap();
        let mut waiters = Vec::new();
        for (i, name) in ["A", "B", "C"].into_iter().enumerate() {
            let (lanes2, order) = (lanes.clone(), order.clone());
            waiters.push(thread::spawn(move || {
                let _seat = lanes2.acquire("s", LONG).unwrap();
                lock(&order).push(name);
            }));
            // Each waiter is in line before the next one comes.
            wait_for("the waiter to take its ticket", || {
                lanes.tickets_taken() == i as u64 + 2
            });
        }
        // Free seats do not let a turn of a busy session in.
        assert_eq!(lanes.running(), 1);
        // The holder lets go and asks again at once: it is behind A, B and C.
        drop(held);
        let again = lanes.acquire("s", LONG).unwrap();
        lock(&order).push("D");
        drop(again);
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(*lock(&order), ["A", "B", "C", "D"]);
    }

    #[test]
    fn tickets_are_served_in_the_order_they_were_taken() {
        // The ticket is taken when the request arrives; the handler may come
        // to wait later than one that arrived after it.
        let lanes = Arc::new(Lanes::new(3));
        let order = Arc::new(Mutex::new(Vec::new()));
        let a = lanes.ticket("a".into());
        let b = lanes.ticket("b".into());
        let second = {
            let (lanes, order) = (lanes.clone(), order.clone());
            thread::spawn(move || {
                let _seat = lanes.wait(b, LONG).unwrap();
                lock(&order).push("B");
            })
        };
        // Limit 1 (no own tabs reported): B has found the seat free but A
        // first in line, and sleeps.
        wait_for("B to wait for its turn", || lanes.parked() >= 1);
        assert!(lock(&order).is_empty(), "B went ahead of A");
        let seat = lanes.wait(a, Duration::from_secs(1)).unwrap();
        lock(&order).push("A");
        drop(seat);
        second.join().unwrap();
        assert_eq!(*lock(&order), ["A", "B"]);
    }

    #[test]
    fn a_ticket_left_unused_leaves_the_line() {
        let lanes = Lanes::new(3);
        let a = lanes.ticket("a".into());
        let b = lanes.ticket("b".into());
        lanes.leave(a);
        assert!(lanes.wait(b, Duration::from_millis(100)).is_some());
    }

    #[test]
    fn a_waiter_that_gives_up_leaves_the_line() {
        let lanes = Arc::new(Lanes::new(3));
        let held = lanes.acquire("a", Duration::from_secs(1)).unwrap();
        assert!(lanes.acquire("b", Duration::from_millis(50)).is_none());
        let next = {
            let lanes = lanes.clone();
            thread::spawn(move || lanes.acquire("c", LONG).is_some())
        };
        // Held's ticket was taken first, the leaver's second, this one's third.
        wait_for("the next one to take its ticket", || {
            lanes.tickets_taken() == 3
        });
        drop(held);
        assert!(next.join().unwrap(), "the one behind the leaver is served");
        assert!(lanes.acquire("d", Duration::from_millis(100)).is_some());
    }

    #[test]
    fn the_limit_is_1_until_own_tabs_are_reported_and_follows_them() {
        let lanes = Lanes::new(3);
        assert_eq!(lanes.limit(), 1, "nothing reported yet");
        lanes.set_own_tabs(Some(8));
        assert_eq!(lanes.limit(), 3, "max_parallel is the smaller");
        lanes.set_own_tabs(Some(2));
        assert_eq!(lanes.limit(), 2, "own tabs are the smaller");
        lanes.set_own_tabs(Some(0));
        assert_eq!(lanes.limit(), 1, "never under 1");
        lanes.set_own_tabs(None);
        assert_eq!(lanes.limit(), 1, "an extension without the field");
        let wide = Lanes::new(100);
        wide.set_own_tabs(Some(100));
        assert_eq!(wide.limit(), 8, "max_parallel is clamped to 8");
        let narrow = Lanes::new(0);
        narrow.set_own_tabs(Some(8));
        assert_eq!(narrow.limit(), 1, "max_parallel is clamped to 1");
    }

    #[test]
    fn different_sessions_run_at_once_up_to_the_limit() {
        let lanes = Arc::new(Lanes::new(2));
        lanes.set_own_tabs(Some(8));
        let a = lanes.acquire("a", LONG).unwrap();
        let b = lanes.acquire("b", LONG).unwrap();
        assert_eq!(lanes.running(), 2);
        // A third session waits for a seat.
        assert!(lanes.acquire("c", Duration::from_millis(50)).is_none());
        let third = {
            let lanes = lanes.clone();
            thread::spawn(move || lanes.acquire("c", LONG).is_some())
        };
        wait_for("C to wait", || lanes.tickets_taken() == 4);
        drop(a);
        assert!(third.join().unwrap(), "C got A's seat");
        drop(b);
    }

    #[test]
    fn a_free_seat_goes_to_the_earliest_waiter_whose_session_is_idle() {
        let lanes = Arc::new(Lanes::new(2));
        lanes.set_own_tabs(Some(2));
        let a = lanes.acquire("a", LONG).unwrap();
        let x = lanes.acquire("x", LONG).unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        // In line: a again (its session is busy), then b.
        let mut waiters = Vec::new();
        for (i, (key, name)) in [("a", "a2"), ("b", "b")].into_iter().enumerate() {
            let (lanes2, order) = (lanes.clone(), order.clone());
            waiters.push(thread::spawn(move || {
                let _seat = lanes2.acquire(key, LONG).unwrap();
                lock(&order).push(name);
            }));
            wait_for("the waiter to take its ticket", || {
                lanes.tickets_taken() == i as u64 + 3
            });
        }
        // X's seat frees: b goes, though a2 came first (a still runs).
        drop(x);
        wait_for("b to get the seat", || lock(&order).len() == 1);
        assert_eq!(*lock(&order), ["b"]);
        drop(a);
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(*lock(&order), ["b", "a2"]);
    }

    #[test]
    fn a_grown_limit_lets_a_waiter_in() {
        let lanes = Arc::new(Lanes::new(3));
        let a = lanes.acquire("a", LONG).unwrap();
        let second = {
            let lanes = lanes.clone();
            thread::spawn(move || lanes.acquire("b", LONG).is_some())
        };
        wait_for("B to wait", || lanes.parked() >= 1);
        lanes.set_own_tabs(Some(8));
        assert!(second.join().unwrap(), "B runs next to A");
        drop(a);
    }
}
