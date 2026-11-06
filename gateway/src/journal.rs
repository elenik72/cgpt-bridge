//! The gateway's records of answers it could not read as meant, kept to find
//! out why a call was missed (`docs/security.md` §15):
//!
//! - `unparsed.jsonl` in the gateway's data folder: one JSON line per answer
//!   that went to OpenCode as text although it mentions calls, per failed
//!   repair round, per answer that came twice over and per answer whose
//!   stream and page text differ. The answer text is redacted (the same
//!   `core` redaction as everything sent to ChatGPT) and then cut. At
//!   [`MAX_LOG_BYTES`] the file becomes `unparsed.jsonl.1` (an older `.1`
//!   is dropped), so the two take at most twice that.
//! - `captures/<time>-<ask id>.sse`, only while `debug_capture_turns` in the
//!   config has turns left: the event stream of every answer of such a
//!   turn as the extension captures it (text pieces as references, joined
//!   once at the end so that a secret split over events is redacted whole),
//!   redacted, never cut (a cut could halve a secret in the joined text):
//!   one over [`MAX_CAPTURE_BYTES`] is not written, and the log says so.
//!
//! Files and the `captures` folder are owner-only on Unix (like the config);
//! on Windows they keep the ACL of the user profile. A record that cannot be
//! written is a log line, never a failed turn.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use cgpt_bridge_core::redact::redact;
use cgpt_bridge_protocol::StreamCheck;
use serde_json::{json, Value};

use crate::log::log;

pub const UNPARSED_FILE: &str = "unparsed.jsonl";
pub const CAPTURES_DIR: &str = "captures";
/// Size of `unparsed.jsonl` at which it is rotated.
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
/// Characters of an answer kept in a record: the start and the end (where a
/// call block usually is), the middle cut out.
pub const MAX_TEXT_CHARS: usize = 16 * 1024;
const TAIL_CHARS: usize = 4 * 1024;
/// Characters kept of the answer in a stream/page mismatch record (start
/// and end), and how many such records an hour are written at most: the
/// records of missed calls must not be rotated away by them.
pub const MAX_MISMATCH_TEXT_CHARS: usize = 1024;
const MISMATCH_TAIL_CHARS: usize = 256;
pub const MAX_MISMATCH_RECORDS_PER_HOUR: u32 = 20;
const HOUR_MS: u64 = 60 * 60 * 1000;
/// Characters of a record's `detail` (the parser's or the bridge's words).
const MAX_DETAIL_CHARS: usize = 1_000;
/// Bytes of one capture, redacted, written to `captures/`. The extension
/// keeps at most 512 Ki UTF-16 units, which in UTF-8 take up to 3 bytes each
/// (Cyrillic 2), and its frame to the host is at most 1 MiB: this is above
/// both with room for redaction markers, so a capture from the extension is
/// never refused for its size; anything larger is not written at all.
pub const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

/// Why an answer is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The final answer went to OpenCode as text, and it mentions calls.
    CallLikeText,
    /// The repair round did not give usable calls either.
    RepairFailed,
    /// The answer came twice over; the calls of one copy were taken.
    DoubledAnswer,
    /// The answer read from the stream and the one the page shows differ.
    StreamPageMismatch,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::CallLikeText => "call_like_text",
            Reason::RepairFailed => "repair_failed",
            Reason::DoubledAnswer => "doubled_answer",
            Reason::StreamPageMismatch => "stream_page_mismatch",
        }
    }
}

/// One record of `unparsed.jsonl`.
pub struct Record<'a> {
    pub session: &'a str,
    pub ask_id: &'a str,
    pub reason: Reason,
    /// The parser's problem or the like; may be empty.
    pub detail: &'a str,
    /// The answer as ChatGPT gave it.
    pub text: &'a str,
    /// For a failed repair: the answer the repair followed.
    pub first_text: Option<&'a str>,
    pub stream_check: Option<&'a StreamCheck>,
}

pub struct Journal {
    dir: PathBuf,
    /// One record (or rotation) at a time: turns of several sessions run at
    /// once.
    write: Mutex<WriteState>,
    /// Turns whose raw answer streams are still to be captured.
    capture_turns_left: AtomicU32,
    /// See [`MAX_LOG_BYTES`].
    max_bytes: u64,
}

#[derive(Default)]
struct WriteState {
    /// The last rotation failed (Windows: the file held open): logged once,
    /// records go on into the file, the next write tries again.
    rotation_failed: bool,
    /// The hour the mismatch records are counted in: its start, how many
    /// were written and how many dropped.
    mismatch_hour_ms: u64,
    mismatch_written: u32,
    mismatch_dropped: u32,
}

impl Journal {
    /// Records in `dir` (the gateway's data folder); the raw streams of the
    /// next `capture_turns` turns go to `dir/captures`.
    pub fn new(dir: PathBuf, capture_turns: u32) -> Journal {
        Journal {
            dir,
            write: Mutex::new(WriteState::default()),
            capture_turns_left: AtomicU32::new(capture_turns),
            max_bytes: MAX_LOG_BYTES,
        }
    }

    #[cfg(test)]
    fn with_max_bytes(mut self, max_bytes: u64) -> Journal {
        self.max_bytes = max_bytes;
        self
    }

    /// Whether this turn's answers are captured; counts the turn off.
    pub fn take_capture_turn(&self) -> bool {
        let mut left = self.capture_turns_left.load(Ordering::SeqCst);
        while left > 0 {
            match self.capture_turns_left.compare_exchange(
                left,
                left - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(now) => left = now,
            }
        }
        false
    }

    /// A turn taken with [`take_capture_turn`](Self::take_capture_turn)
    /// whose ask never reached the bridge: it counts again.
    pub fn give_back_capture_turn(&self) {
        self.capture_turns_left.fetch_add(1, Ordering::SeqCst);
    }

    /// Append `record` to `unparsed.jsonl`, rotating it first when it would
    /// grow past [`MAX_LOG_BYTES`]. Stream/page mismatches are written at
    /// most [`MAX_MISMATCH_RECORDS_PER_HOUR`] an hour; the next one written
    /// counts the dropped ones.
    pub fn unparsed(&self, record: Record) {
        self.unparsed_at(record, crate::session::now_ms() as u64)
    }

    fn unparsed_at(&self, record: Record, now_ms: u64) {
        let mut state = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut dropped_before = 0;
        if record.reason == Reason::StreamPageMismatch {
            if now_ms.saturating_sub(state.mismatch_hour_ms) >= HOUR_MS {
                state.mismatch_hour_ms = now_ms;
                state.mismatch_written = 0;
            }
            if state.mismatch_written >= MAX_MISMATCH_RECORDS_PER_HOUR {
                if state.mismatch_dropped == 0 {
                    log(format_args!(
                        "cgpt-gateway: more than {} stream/page mismatches this hour: \
                         the next ones are counted, not recorded",
                        MAX_MISMATCH_RECORDS_PER_HOUR
                    ));
                }
                state.mismatch_dropped += 1;
                return;
            }
            state.mismatch_written += 1;
            dropped_before = std::mem::take(&mut state.mismatch_dropped);
        }
        let line = record_line(&record, now_ms, dropped_before);
        if let Err(e) = self.append(&line, &mut state) {
            log(format_args!(
                "cgpt-gateway: {} not written: {}",
                self.dir.join(UNPARSED_FILE).display(),
                e
            ));
        }
    }

    fn append(&self, line: &str, state: &mut WriteState) -> io::Result<()> {
        let path = self.dir.join(UNPARSED_FILE);
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len > 0 && len + line.len() as u64 + 1 > self.max_bytes {
            match std::fs::rename(&path, self.dir.join(format!("{}.1", UNPARSED_FILE))) {
                Ok(()) => state.rotation_failed = false,
                Err(e) => {
                    if !state.rotation_failed {
                        log(format_args!(
                            "cgpt-gateway: {} not rotated ({}): written on past its size, \
                             tried again at the next record",
                            path.display(),
                            e
                        ));
                    }
                    state.rotation_failed = true;
                }
            }
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut file = open_private(&path, true)?;
        file.write_all(format!("{}\n", line).as_bytes())
    }

    /// Write one answer's capture, redacted and whole, to
    /// `captures/<time>-<ask id>.sse`; one over [`MAX_CAPTURE_BYTES`] is
    /// dropped with a log line, never cut.
    pub fn capture(&self, ask_id: &str, raw: &str) {
        let dir = self.dir.join(CAPTURES_DIR);
        let name = format!(
            "{}-{}.sse",
            file_stamp(crate::session::now_ms() as u64),
            safe_name(ask_id)
        );
        let text = redact(raw, "").stdout;
        if text.len() > MAX_CAPTURE_BYTES {
            log(format_args!(
                "cgpt-gateway: debug: answer stream of [{}] not written: {} bytes, more than {}",
                ask_id,
                text.len(),
                MAX_CAPTURE_BYTES
            ));
            return;
        }
        let result = make_private_dir(&dir)
            .and_then(|()| open_private(&dir.join(&name), false)?.write_all(text.as_bytes()));
        match result {
            Ok(()) => log(format_args!(
                "cgpt-gateway: debug: answer stream of [{}] written to {}",
                ask_id,
                dir.join(&name).display()
            )),
            Err(e) => log(format_args!(
                "cgpt-gateway: debug: answer stream of [{}] not written: {}",
                ask_id, e
            )),
        }
    }
}

/// The JSON line of a record at `ms`; `dropped_before`: mismatch records
/// dropped since the last one written.
fn record_line(r: &Record, ms: u64, dropped_before: u32) -> String {
    let mut value = json!({
        "time": cgpt_bridge_core::time::format_utc_z(ms),
        "session": r.session,
        "ask_id": r.ask_id,
        "reason": r.reason.as_str(),
    });
    let obj = value.as_object_mut().expect("an object");
    if dropped_before > 0 {
        obj.insert("dropped_before".into(), Value::from(dropped_before));
    }
    // A mismatch says little through the text: keep only a little of it.
    let (max, tail) = if r.reason == Reason::StreamPageMismatch {
        (MAX_MISMATCH_TEXT_CHARS, MISMATCH_TAIL_CHARS)
    } else {
        (MAX_TEXT_CHARS, TAIL_CHARS)
    };
    if !r.detail.is_empty() {
        let detail = redact(r.detail, "").stdout;
        obj.insert(
            "detail".into(),
            Value::String(detail.chars().take(MAX_DETAIL_CHARS).collect()),
        );
    }
    for (key, text) in [("text", Some(r.text)), ("first_text", r.first_text)] {
        let Some(text) = text else { continue };
        // Redacted before it is cut: a secret cut in half would no longer
        // match.
        let redacted = redact(text, "").stdout;
        obj.insert(key.into(), Value::String(cut_middle(&redacted, max, tail)));
        obj.insert(
            format!("{}_chars", key),
            Value::from(text.chars().count() as u64),
        );
    }
    if let Some(check) = r.stream_check {
        obj.insert(
            "stream_check".into(),
            serde_json::to_value(check).unwrap_or(Value::Null),
        );
    }
    value.to_string()
}

/// `text` cut to `max` characters: its start and its last `tail` with a
/// marker between them.
fn cut_middle(text: &str, max: usize, tail: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let head = max - tail;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(n - tail).collect();
    format!("{}\n…[{} characters cut]…\n{}", start, n - head - tail, end)
}

/// `YYYYMMDDTHHMMSSZ`, for file names.
fn file_stamp(ms: u64) -> String {
    let (y, mo, d, h, mi, s) = cgpt_bridge_core::time::ymdhms_utc(ms / 1000);
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", y, mo, d, h, mi, s)
}

/// An id as a file name: letters, digits, `_` and `-` only.
fn safe_name(id: &str) -> String {
    id.chars()
        .take(100)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A file readable by the owner only on Unix (mode 600): appended to, or
/// created new.
fn open_private(path: &Path, append: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    if append {
        options.append(true).create(true);
    } else {
        options.write(true).create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// A folder only the owner may enter on Unix (mode 700).
fn make_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::temp_dir;

    fn record<'a>(text: &'a str, reason: Reason) -> Record<'a> {
        Record {
            session: "ses_1",
            ask_id: "gw_ask_1",
            reason,
            detail: "блок «json»",
            text,
            first_text: None,
            stream_check: None,
        }
    }

    fn lines(dir: &Path, name: &str) -> Vec<Value> {
        std::fs::read_to_string(dir.join(name))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn a_record_is_one_redacted_json_line() {
        let dir = temp_dir("journal-record");
        let j = Journal::new(dir.clone(), 0);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        j.unparsed(Record {
            first_text: Some("первый ответ"),
            stream_check: Some(&StreamCheck {
                stream_chars: 10,
                page_chars: 20,
                mismatch: true,
            }),
            ..record(
                &format!("```json\n{{\"key\": \"{}\"}}\n```", secret),
                Reason::RepairFailed,
            )
        });
        j.unparsed(record("второй", Reason::CallLikeText));
        let got = lines(&dir, UNPARSED_FILE);
        assert_eq!(got.len(), 2);
        let r = &got[0];
        assert_eq!(r["session"], "ses_1");
        assert_eq!(r["ask_id"], "gw_ask_1");
        assert_eq!(r["reason"], "repair_failed");
        assert_eq!(r["detail"], "блок «json»");
        assert_eq!(r["first_text"], "первый ответ");
        assert_eq!(r["stream_check"]["page_chars"], 20);
        let text = r["text"].as_str().unwrap();
        assert!(!text.contains(secret), "{}", text);
        assert!(text.contains("«redacted:"), "{}", text);
        assert!(r["time"].as_str().unwrap().ends_with('Z'));
        assert_eq!(got[1]["reason"], "call_like_text");
        assert!(got[1].get("first_text").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(UNPARSED_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn long_answers_keep_their_start_and_end_and_are_redacted_first() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        // The secret straddles the cut: redaction before cutting removes it.
        let long = format!(
            "начало {}{}{} конец",
            "а".repeat(MAX_TEXT_CHARS - TAIL_CHARS - 10),
            secret,
            "б".repeat(TAIL_CHARS)
        );
        let line = record_line(&record(&long, Reason::CallLikeText), 0, 0);
        let r: Value = serde_json::from_str(&line).unwrap();
        let text = r["text"].as_str().unwrap();
        assert!(text.starts_with("начало"), "{}", &text[..20]);
        assert!(text.ends_with("конец"));
        assert!(text.contains("characters cut]"));
        assert!(!text.contains("AKIA"), "a half secret is left");
        assert_eq!(r["text_chars"], long.chars().count() as u64);
    }

    #[test]
    fn the_log_rotates_into_one_older_file() {
        let dir = temp_dir("journal-rotate");
        let j = Journal::new(dir.clone(), 0);
        let big = "x".repeat(MAX_TEXT_CHARS);
        // Each record is about 16 KiB: enough of them pass 2 MiB.
        let per = record_line(&record(&big, Reason::CallLikeText), 0, 0).len() as u64 + 1;
        let n = MAX_LOG_BYTES / per + 2;
        for _ in 0..n {
            j.unparsed(record(&big, Reason::CallLikeText));
        }
        let now = std::fs::metadata(dir.join(UNPARSED_FILE)).unwrap().len();
        let old = std::fs::metadata(dir.join(format!("{}.1", UNPARSED_FILE)))
            .unwrap()
            .len();
        assert!(
            now <= MAX_LOG_BYTES && old <= MAX_LOG_BYTES,
            "{} {}",
            now,
            old
        );
        assert_eq!((now + old) / per, n);
        // A second rotation drops the oldest file: never more than two.
        for _ in 0..n {
            j.unparsed(record(&big, Reason::CallLikeText));
        }
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 2, "{:?}", files);
    }

    #[test]
    fn captures_count_down_and_are_redacted_and_capped() {
        let dir = temp_dir("journal-capture");
        let j = Journal::new(dir.clone(), 2);
        assert!(j.take_capture_turn());
        assert!(j.take_capture_turn());
        assert!(!j.take_capture_turn());
        assert!(!Journal::new(dir.clone(), 0).take_capture_turn());

        // As much Cyrillic as the extension keeps (512 Ki UTF-16 units, about
        // 1 MiB in UTF-8), the joined text at its end: written whole.
        let tail = "конец склеенного текста";
        let raw = format!(
            "data: {{\"v\": \"AKIAIOSFODNN7EXAMPLE\"}}\n\n{}{}",
            "я".repeat(512 * 1024 - 64),
            tail
        );
        j.capture("gw_ask/../x", &raw);
        let files: Vec<_> = std::fs::read_dir(dir.join(CAPTURES_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with("-gw_ask____x.sse"), "{}", name);
        let text = std::fs::read_to_string(&files[0]).unwrap();
        assert!(text.ends_with(tail), "never cut");
        assert!(!text.contains("AKIA"), "{}", &text[..60]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&files[0]), 0o600);
            assert_eq!(mode(&dir.join(CAPTURES_DIR)), 0o700);
        }
    }

    #[test]
    fn a_capture_too_big_is_dropped_whole_not_cut() {
        let dir = temp_dir("journal-capture-big");
        let j = Journal::new(dir.clone(), 1);
        j.capture("gw_ask_big", &"ж".repeat(MAX_CAPTURE_BYTES / 2 + 1));
        assert!(
            !dir.join(CAPTURES_DIR).exists()
                || std::fs::read_dir(dir.join(CAPTURES_DIR)).unwrap().count() == 0
        );
    }

    #[test]
    fn mismatch_records_are_short_and_at_most_20_an_hour() {
        let dir = temp_dir("journal-mismatch-limit");
        let j = Journal::new(dir.clone(), 0);
        let long = "я".repeat(10_000);
        let hour = 1_800_000_000_000;
        for i in 0..25 {
            j.unparsed_at(record(&long, Reason::StreamPageMismatch), hour + i);
        }
        // Other records are never dropped.
        j.unparsed_at(record("x", Reason::RepairFailed), hour + 30);
        // The next hour: written again, with the dropped ones counted.
        j.unparsed_at(record(&long, Reason::StreamPageMismatch), hour + HOUR_MS);
        let got = lines(&dir, UNPARSED_FILE);
        assert_eq!(got.len(), 22);
        let text = got[0]["text"].as_str().unwrap();
        assert!(
            text.chars().count() < MAX_MISMATCH_TEXT_CHARS + 40,
            "{}",
            text.len()
        );
        assert_eq!(got[0]["text_chars"], 10_000);
        assert_eq!(got[20]["reason"], "repair_failed");
        assert_eq!(got[21]["dropped_before"], 5);
        assert!(got[1].get("dropped_before").is_none());
    }

    #[test]
    fn a_failed_rotation_writes_on_and_is_tried_again() {
        let dir = temp_dir("journal-rotate-fail");
        let j = Journal::new(dir.clone(), 0).with_max_bytes(1_000);
        // A non-empty folder named like the older file: the rename fails.
        let older = dir.join(format!("{}.1", UNPARSED_FILE));
        std::fs::create_dir_all(older.join("held")).unwrap();
        let line = "z".repeat(400);
        for _ in 0..4 {
            j.unparsed(record(&line, Reason::CallLikeText));
        }
        assert_eq!(
            lines(&dir, UNPARSED_FILE).len(),
            4,
            "written on past the size"
        );
        std::fs::remove_dir_all(&older).unwrap();
        j.unparsed(record(&line, Reason::CallLikeText));
        assert_eq!(lines(&dir, UNPARSED_FILE).len(), 1);
        assert_eq!(std::fs::read_to_string(&older).unwrap().lines().count(), 4);
    }

    #[test]
    fn two_turns_writing_at_once_keep_every_line_whole_through_a_rotation() {
        let dir = temp_dir("journal-threads");
        let j = std::sync::Arc::new(Journal::new(dir.clone(), 0).with_max_bytes(64 * 1024));
        let text = "w".repeat(900);
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (j, text) = (j.clone(), text.clone());
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        j.unparsed(record(&text, Reason::CallLikeText));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // 100 lines of about 1 KiB: one rotation, nothing lost, all JSON.
        let now = lines(&dir, UNPARSED_FILE);
        let old = lines(&dir, &format!("{}.1", UNPARSED_FILE));
        assert_eq!(now.len() + old.len(), 100);
        assert!(!old.is_empty());
        assert!(now.iter().chain(&old).all(|r| r["text"] == text.as_str()));
    }
}
