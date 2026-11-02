//! The one input path of `cgpt ask` and `cgpt agent`: words on the command
//! line, then the clipboard (`--buffer`) or piped stdin, then `$EDITOR`
//! (`--editor`). The two commands differ only in the usage hint.
//!
//! The collected text is redacted here, once, before it reaches the editor,
//! the socket or `.cgpt-bridge/` (`--no-redact` skips that).

use std::io::{self, IsTerminal, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::clipboard;
use crate::editor;
use crate::exit::ExitKind;
use crate::redact;

/// Where the secondary input comes from. A trait so tests need neither a
/// real terminal, clipboard nor editor.
pub trait InputEnv {
    fn stdin_is_terminal(&self) -> bool;
    fn read_stdin(&mut self) -> io::Result<String>;
    fn read_clipboard(&mut self) -> io::Result<String>;
    fn edit(&mut self, initial: &str) -> io::Result<String>;
}

pub struct RealEnv;

/// How long a piped stdin may stay open and silent before cgpt says that it
/// is waiting for it.
const STDIN_HINT_AFTER: Duration = Duration::from_secs(5);

/// Said once on stderr after `STDIN_HINT_AFTER` of reading stdin.
const STDIN_HINT: &str =
    "cgpt: still reading the prompt from stdin (it is not a terminal and is still open). \
Close it to go on: end the pipe, or run with < /dev/null (Windows: < NUL) to send only the words.";

impl InputEnv for RealEnv {
    fn stdin_is_terminal(&self) -> bool {
        io::stdin().is_terminal()
    }
    fn read_stdin(&mut self) -> io::Result<String> {
        // A piped stdin that nobody writes to and nobody closes (a script or
        // a background job) would otherwise keep cgpt waiting in silence.
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        thread::spawn(move || {
            thread::sleep(STDIN_HINT_AFTER);
            if !flag.load(Ordering::SeqCst) {
                eprintln!("{}", STDIN_HINT);
            }
        });
        let mut buf = String::new();
        let read = io::stdin().lock().read_to_string(&mut buf);
        done.store(true, Ordering::SeqCst);
        read?;
        Ok(buf)
    }
    fn read_clipboard(&mut self) -> io::Result<String> {
        clipboard::read()
    }
    fn edit(&mut self, initial: &str) -> io::Result<String> {
        editor::capture(initial)
    }
}

/// Redact the text a person hands to `cgpt ask` / `cgpt agent` with the
/// same rules as command output. Returns the text and the names of the rules
/// that fired (empty when nothing changed).
pub fn mask_secrets(text: &str) -> (String, Vec<String>) {
    let r = redact::redact(text, "");
    (r.stdout, r.rules_fired)
}

/// The one stderr line that says input was redacted; names rules only, never
/// values. `None` when nothing fired.
pub fn redaction_notice(rules: &[String]) -> Option<String> {
    if rules.is_empty() {
        return None;
    }
    Some(format!(
        "cgpt: input redacted before sending (rules: {}). Send as is: --no-redact. Review first: --editor.",
        rules.join(", ")
    ))
}

/// Combine positional words, the secondary source and the editor pass into
/// the text to send. Source resolution:
/// - `--buffer`: secondary is the OS clipboard, stdin untouched.
/// - otherwise: secondary is stdin when piped (non-TTY).
/// - `--editor`: the combined text is the editor's initial template, and the
///   saved buffer wins.
///
/// With `redact` the combined text is redacted before the editor sees it
/// (the person reviews what will be sent); what they save is not touched.
///
/// Exits: no text at all → `Usage` (2) with `usage` on stderr; a clipboard,
/// stdin or editor failure → `Generic` (1).
pub fn collect(
    positional: &[String],
    buffer: bool,
    editor_flag: bool,
    redact: bool,
    usage: &str,
) -> Result<String, ExitKind> {
    collect_with(&mut RealEnv, positional, buffer, editor_flag, redact, usage)
}

pub fn collect_with(
    env: &mut dyn InputEnv,
    positional: &[String],
    buffer: bool,
    editor_flag: bool,
    redact: bool,
    usage: &str,
) -> Result<String, ExitKind> {
    let words = if positional.is_empty() {
        None
    } else {
        Some(positional.join(" "))
    };

    let secondary = if buffer {
        match env.read_clipboard() {
            Ok(s) if !s.trim().is_empty() => Some(s),
            Ok(_) => None,
            Err(e) => {
                eprintln!("cgpt: --buffer: failed to read clipboard: {}", e);
                return Err(ExitKind::Generic);
            }
        }
    } else if env.stdin_is_terminal() {
        None
    } else {
        match env.read_stdin() {
            Ok(s) if !s.trim().is_empty() => Some(s),
            Ok(_) => None,
            Err(e) => {
                eprintln!("cgpt: failed to read stdin: {}", e);
                return Err(ExitKind::Generic);
            }
        }
    };

    let mut combined = match (words, secondary) {
        (Some(a), Some(b)) => format!("{}\n\n{}", a, b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => String::new(),
    };

    if redact {
        let (masked, rules) = mask_secrets(&combined);
        if let Some(notice) = redaction_notice(&rules) {
            eprintln!("{}", notice);
        }
        combined = masked;
    }

    if editor_flag {
        combined = match env.edit(&combined) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cgpt: --editor: {}", e);
                return Err(ExitKind::Generic);
            }
        };
    }

    if combined.trim().is_empty() {
        eprintln!("{}", usage);
        return Err(ExitKind::Usage);
    }

    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        tty: bool,
        stdin: Option<io::Result<String>>,
        clipboard: Option<io::Result<String>>,
        edited: Option<io::Result<String>>,
        /// What the editor was handed.
        seen_by_editor: Option<String>,
        stdin_reads: u32,
        clipboard_reads: u32,
    }

    fn err() -> io::Error {
        io::Error::other("nope")
    }

    impl InputEnv for Fake {
        fn stdin_is_terminal(&self) -> bool {
            self.tty
        }
        fn read_stdin(&mut self) -> io::Result<String> {
            self.stdin_reads += 1;
            self.stdin.take().unwrap_or_else(|| Ok(String::new()))
        }
        fn read_clipboard(&mut self) -> io::Result<String> {
            self.clipboard_reads += 1;
            self.clipboard.take().unwrap_or_else(|| Ok(String::new()))
        }
        fn edit(&mut self, initial: &str) -> io::Result<String> {
            self.seen_by_editor = Some(initial.to_string());
            self.edited
                .take()
                .unwrap_or_else(|| Ok(initial.to_string()))
        }
    }

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    fn run(env: &mut Fake, w: &[&str], buffer: bool, editor: bool) -> Result<String, ExitKind> {
        collect_with(env, &words(w), buffer, editor, false, "usage hint")
    }

    fn run_redacted(
        env: &mut Fake,
        w: &[&str],
        buffer: bool,
        editor: bool,
    ) -> Result<String, ExitKind> {
        collect_with(env, &words(w), buffer, editor, true, "usage hint")
    }

    const FAKE_KEY: &str = "sk-abcdefghijklmnopqrstuvwxyz0123456789";

    #[test]
    fn words_alone_are_joined_with_spaces() {
        let mut env = Fake {
            tty: true,
            ..Default::default()
        };
        assert_eq!(run(&mut env, &["a", "b"], false, false), Ok("a b".into()));
        assert_eq!(env.stdin_reads, 0, "a terminal stdin is never read");
    }

    #[test]
    fn piped_stdin_alone_is_the_text() {
        let mut env = Fake {
            stdin: Some(Ok("piped\n".into())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &[], false, false), Ok("piped\n".into()));
    }

    #[test]
    fn words_and_stdin_are_joined_by_a_blank_line() {
        let mut env = Fake {
            stdin: Some(Ok("log".into())),
            ..Default::default()
        };
        assert_eq!(
            run(&mut env, &["explain"], false, false),
            Ok("explain\n\nlog".into())
        );
    }

    #[test]
    fn buffer_reads_the_clipboard_and_leaves_stdin_alone() {
        let mut env = Fake {
            stdin: Some(Ok("ignored".into())),
            clipboard: Some(Ok("clip".into())),
            ..Default::default()
        };
        assert_eq!(
            run(&mut env, &["see"], true, false),
            Ok("see\n\nclip".into())
        );
        assert_eq!(env.stdin_reads, 0);
    }

    #[test]
    fn blank_stdin_or_clipboard_counts_as_no_input() {
        let mut env = Fake {
            stdin: Some(Ok("  \n".into())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &[], false, false), Err(ExitKind::Usage));
        let mut env = Fake {
            clipboard: Some(Ok("\n".into())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &[], true, false), Err(ExitKind::Usage));
        assert_eq!(env.clipboard_reads, 1);
    }

    #[test]
    fn empty_input_is_a_usage_error() {
        let mut env = Fake {
            tty: true,
            ..Default::default()
        };
        assert_eq!(run(&mut env, &[], false, false), Err(ExitKind::Usage));
    }

    #[test]
    fn clipboard_stdin_and_editor_failures_are_generic() {
        let mut env = Fake {
            clipboard: Some(Err(err())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &["x"], true, false), Err(ExitKind::Generic));
        let mut env = Fake {
            stdin: Some(Err(err())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &["x"], false, false), Err(ExitKind::Generic));
        let mut env = Fake {
            tty: true,
            edited: Some(Err(err())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &["x"], false, true), Err(ExitKind::Generic));
    }

    #[test]
    fn editor_gets_the_combined_text_and_its_buffer_wins() {
        let mut env = Fake {
            stdin: Some(Ok("log".into())),
            edited: Some(Ok("final text".into())),
            ..Default::default()
        };
        assert_eq!(
            run(&mut env, &["explain"], false, true),
            Ok("final text".into())
        );
        assert_eq!(env.seen_by_editor.as_deref(), Some("explain\n\nlog"));
    }

    #[test]
    fn an_emptied_editor_buffer_is_a_usage_error() {
        let mut env = Fake {
            tty: true,
            edited: Some(Ok("  ".into())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &["x"], false, true), Err(ExitKind::Usage));
    }

    #[test]
    fn mask_secrets_replaces_secrets_and_names_the_rules() {
        let text = format!("API_KEY=hunter2-very-secret\nnote {FAKE_KEY}\n");
        let (masked, rules) = mask_secrets(&text);
        assert!(!masked.contains("hunter2-very-secret"), "{}", masked);
        assert!(!masked.contains(FAKE_KEY), "{}", masked);
        assert!(masked.contains("«redacted:"), "{}", masked);
        assert!(rules.contains(&"openai_api_key".to_string()), "{:?}", rules);
        assert!(rules.len() >= 2, "{:?}", rules);
    }

    #[test]
    fn mask_secrets_leaves_plain_text_and_code_alone() {
        for text in [
            "",
            "explain this failure",
            "let password = read_password()?;\n",
            "pub token: Option<String>,",
        ] {
            let (masked, rules) = mask_secrets(text);
            assert_eq!(masked, text);
            assert!(rules.is_empty(), "{}: {:?}", text, rules);
        }
    }

    #[test]
    fn the_notice_names_rules_without_values_and_is_silent_when_none_fired() {
        assert_eq!(redaction_notice(&[]), None);
        let n = redaction_notice(&["jwt_like".into(), "key_value_secret".into()]).unwrap();
        assert_eq!(
            n,
            "cgpt: input redacted before sending (rules: jwt_like, key_value_secret). \
             Send as is: --no-redact. Review first: --editor."
        );
    }

    #[test]
    fn collect_redacts_words_stdin_and_clipboard_but_not_when_off() {
        let secret_line = format!("token note {FAKE_KEY}");
        let mut env = Fake {
            stdin: Some(Ok(secret_line.clone())),
            ..Default::default()
        };
        let got = run_redacted(&mut env, &["explain"], false, false).unwrap();
        assert_eq!(got, "explain\n\ntoken note «redacted:openai_api_key»");

        let mut env = Fake {
            clipboard: Some(Ok(secret_line.clone())),
            ..Default::default()
        };
        let got = run_redacted(&mut env, &[], true, false).unwrap();
        assert!(!got.contains(FAKE_KEY), "{}", got);

        let mut env = Fake {
            tty: true,
            ..Default::default()
        };
        let got = run_redacted(&mut env, &[FAKE_KEY], false, false).unwrap();
        assert_eq!(got, "«redacted:openai_api_key»");

        // Off: the text is exactly what was given.
        let mut env = Fake {
            stdin: Some(Ok(secret_line.clone())),
            ..Default::default()
        };
        assert_eq!(run(&mut env, &[], false, false), Ok(secret_line));
    }

    #[test]
    fn the_editor_is_handed_redacted_text_and_what_it_saves_is_kept_as_is() {
        let mut env = Fake {
            tty: true,
            edited: Some(Ok(format!("restored {FAKE_KEY}"))),
            ..Default::default()
        };
        let got = run_redacted(&mut env, &[FAKE_KEY], false, true).unwrap();
        assert_eq!(
            env.seen_by_editor.as_deref(),
            Some("«redacted:openai_api_key»")
        );
        assert_eq!(
            got,
            format!("restored {FAKE_KEY}"),
            "the person saw and chose it"
        );
    }

    #[test]
    fn blank_input_is_still_a_usage_error_with_redaction_on() {
        let mut env = Fake {
            tty: true,
            ..Default::default()
        };
        assert_eq!(
            run_redacted(&mut env, &[], false, false),
            Err(ExitKind::Usage)
        );
    }
}
