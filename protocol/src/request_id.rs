//! Request-id generator shared by every client of the host (CLI, gateway).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Generate a request id: `<prefix>_<pid>_<millis>_<counter>`, the numbers in
/// lowercase hex. The host keys its pending map by id, so a reused id would
/// orphan a waiter. The pid keeps two processes sharing one host apart even
/// within the same millisecond, the counter keeps calls of one process apart.
/// Not crypto-strength. The host and the extension treat the id as an opaque
/// string. By construction (short `[A-Za-z0-9_]` prefix plus hex numbers) an
/// id stays within `[A-Za-z0-9_.-]{1,64}`.
pub fn new_request_id(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format_request_id(prefix, std::process::id(), ms, n)
}

fn format_request_id(prefix: &str, pid: u32, ms: u128, n: u32) -> String {
    format!("{}_{:x}_{:x}_{:x}", prefix, pid, ms, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::{Command, Stdio};

    const CHILD_ENV: &str = "CGPT_REQUEST_ID_CHILD";

    fn safe(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    }

    #[test]
    fn id_keeps_the_established_format() {
        // `<prefix>_<pid>_<millis>_<counter>`, all hex: the format CLI and
        // gateway used before the generator moved here.
        assert_eq!(format_request_id("ask", 0x1f, 0xabc, 2), "ask_1f_abc_2");
        let id = new_request_id("gw_ask");
        let parts: Vec<&str> = id.splitn(3, '_').collect();
        assert_eq!(&parts[..2], ["gw", "ask"]);
        let tail: Vec<&str> = id["gw_ask_".len()..].split('_').collect();
        assert_eq!(tail.len(), 3, "{id}");
        assert_eq!(
            u32::from_str_radix(tail[0], 16).unwrap(),
            std::process::id()
        );
        assert!(
            tail[1..]
                .iter()
                .all(|p| u128::from_str_radix(p, 16).is_ok()),
            "{id}"
        );
    }

    #[test]
    fn id_uses_the_safe_alphabet_for_every_prefix_in_use() {
        for prefix in ["ask", "agent", "gw_ask", "gw_stop", "gw_status"] {
            let id = new_request_id(prefix);
            assert!(safe(&id), "{id}");
            assert!(id.starts_with(&format!("{prefix}_")), "{id}");
        }
        // Even the largest counter and a far-future clock stay within 64.
        assert!(safe(&format_request_id(
            "gw_status",
            u32::MAX,
            u128::from(u64::MAX),
            u32::MAX
        )));
    }

    #[test]
    fn same_millisecond_and_counter_in_two_processes_differ_by_pid() {
        assert_ne!(
            format_request_id("ask", 100, 5, 0),
            format_request_id("ask", 101, 5, 0)
        );
    }

    #[test]
    fn ids_are_unique_across_a_burst_from_many_threads() {
        const THREADS: usize = 16;
        const PER_THREAD: usize = 2_000;
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..PER_THREAD)
                        .map(|_| new_request_id("ask"))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut seen = HashSet::new();
        for h in handles {
            for id in h.join().unwrap() {
                assert!(safe(&id), "{id}");
                assert!(seen.insert(id.clone()), "duplicate id {id}");
            }
        }
        assert_eq!(seen.len(), THREADS * PER_THREAD);
    }

    /// Child side of `ids_are_unique_across_processes`: prints ids when the
    /// parent asks for it, a no-op in a normal run.
    #[test]
    fn child_prints_ids() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        for _ in 0..50 {
            println!("RID:{}", new_request_id("ask"));
        }
    }

    #[test]
    fn ids_are_unique_across_processes() {
        const CHILDREN: usize = 4;
        const PER_CHILD: usize = 50;
        let exe = std::env::current_exe().unwrap();
        // Start every child before waiting for any, so their runs overlap.
        let children: Vec<_> = (0..CHILDREN)
            .map(|_| {
                Command::new(&exe)
                    .args([
                        "--exact",
                        "request_id::tests::child_prints_ids",
                        "--nocapture",
                    ])
                    .env(CHILD_ENV, "1")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let mut seen = HashSet::new();
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let mut count = 0;
            for line in String::from_utf8(out.stdout).unwrap().lines() {
                // libtest may print `test <name> ... ` on the same line
                // before the first id (single-threaded runs).
                if let Some(i) = line.find("RID:") {
                    let id = &line[i + 4..];
                    assert!(safe(id), "{id}");
                    assert!(seen.insert(id.to_string()), "duplicate id {id}");
                    count += 1;
                }
            }
            assert_eq!(count, PER_CHILD);
        }
        assert_eq!(seen.len(), CHILDREN * PER_CHILD);
    }
}
