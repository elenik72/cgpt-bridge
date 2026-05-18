//! Command-line argument parsing. Kept separate so it can grow into
//! `agent`/`doctor` subcommands in later milestones without bloating main.rs.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "cgpt",
    about = "Terminal bridge to your active ChatGPT tab",
    long_about = None,
    version,
)]
pub struct Cli {
    /// Override the default native-host socket path. Useful for tests and
    /// for running multiple installs side by side.
    #[arg(long = "socket", value_name = "PATH", global = true)]
    pub socket_override: Option<PathBuf>,

    /// Suppress the animated spinner. Phase markers (▶ / ◀ / ✗) are still
    /// printed so a non-interactive run remains traceable.
    #[arg(long = "no-spinner", global = true, default_value_t = false)]
    pub no_spinner: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Send one prompt to the active ChatGPT tab and print the assistant
    /// response to stdout.
    Ask(AskArgs),

    /// Hand a task to ChatGPT and let it propose shell commands one at a
    /// time, each gated by an interactive confirmation. See `docs/protocol.md`.
    Agent(AgentArgs),

    /// List past agent sessions stored under `.cgpt-bridge/runs/`.
    History(HistoryArgs),

    /// Re-render the final markdown of a stored session without contacting
    /// ChatGPT. Pass a session id from `cgpt history`.
    Replay(ReplayArgs),

    /// Shortcut for `cgpt replay <latest>`. Re-renders the most recent
    /// session's final message.
    Last(LastArgs),
}

#[derive(Debug, clap::Args)]
pub struct AskArgs {
    /// Prompt text. May be combined with stdin (positional args, then
    /// `\n\n`, then stdin contents). If neither is given, exits 2.
    pub prompt: Vec<String>,

    /// Per-request timeout in milliseconds. Includes both transport latency
    /// and the time the assistant takes to stabilize its response.
    #[arg(long = "timeout-ms", default_value_t = 120_000)]
    pub timeout_ms: u64,

    /// Read the prompt from the OS clipboard instead of arg/stdin. When set,
    /// stdin is not consumed even if piped. Positional args, if any, are
    /// prepended to the clipboard contents as: `<args>\n\n<clipboard>`.
    /// Uses `pbpaste` on macOS and `wl-paste`/`xclip`/`xsel` on Linux.
    #[arg(long = "buffer", default_value_t = false)]
    pub buffer: bool,

    /// Open `$EDITOR` (fallback: `vi`, then `nano`) on a tmpfile to compose
    /// the prompt. The tmpfile is pre-populated with whatever combination
    /// of positional args / stdin / `--buffer` would have produced, so the
    /// flag also acts as a "review before send" gate.
    #[arg(long = "editor", default_value_t = false)]
    pub editor: bool,

    /// After printing, also copy the assistant response to the OS clipboard
    /// via `pbcopy` (macOS) / `wl-copy`/`xclip`/`xsel` (Linux).
    #[arg(long = "copy", default_value_t = false)]
    pub copy: bool,
}

#[derive(Debug, clap::Args)]
pub struct AgentArgs {
    /// Task description. Combinable with stdin in the same way as `ask`.
    pub task: Vec<String>,

    /// Per-turn transport timeout in milliseconds. Note: this is separate
    /// from per-command `timeout_ms` declared by the assistant.
    #[arg(long = "timeout-ms", default_value_t = 120_000)]
    pub timeout_ms: u64,

    /// Auto-approve commands the **local** classifier marks `read_only`.
    /// Everything else still prompts. Denylist hits are always blocked.
    #[arg(long = "auto-readonly", default_value_t = false)]
    pub auto_readonly: bool,

    /// DANGER: auto-approve *every* non-denylisted command without a prompt.
    /// The local classifier and confirmation panel are still printed so the
    /// run is auditable, but `[r]un` is implicit. Intended for trusted
    /// sandboxes / disposable VMs. Implies `--auto-readonly`.
    #[arg(long = "yolo", default_value_t = false)]
    pub yolo: bool,

    /// Read the task from the OS clipboard instead of arg/stdin. When set,
    /// stdin is not consumed even if piped. Positional args, if any, are
    /// prepended to the clipboard contents as: `<args>\n\n<clipboard>`.
    /// Uses `pbpaste` on macOS and `wl-paste`/`xclip`/`xsel` on Linux.
    #[arg(long = "buffer", default_value_t = false)]
    pub buffer: bool,

    /// Disable pretty markdown rendering of the final assistant message.
    /// By default, when stdout is a TTY, the final `user_message` is
    /// rendered through the built-in `termimad` skin (headers, lists,
    /// tables, code blocks, emphasis). With this flag the raw markdown is
    /// emitted verbatim. Note: piped/redirected stdout already disables
    /// rendering automatically.
    #[arg(long = "no-pretty", default_value_t = false)]
    pub no_pretty: bool,

    /// Open `$EDITOR` (fallback: `vi`, then `nano`) on a tmpfile to compose
    /// the task. Pre-populated with the combined positional args / stdin /
    /// clipboard, mirroring `cgpt ask --editor`.
    #[arg(long = "editor", default_value_t = false)]
    pub editor: bool,

    /// After the final message is printed, also copy it to the OS clipboard.
    #[arg(long = "copy", default_value_t = false)]
    pub copy: bool,

    /// Continue the most recent agent session in this project: reuse its
    /// `session_id`, skip the prompt contract preamble (ChatGPT already has
    /// the context in its tab), and append further turns to the same
    /// `plan.jsonl` + `runs/<id>/`. Equivalent to `--resume $(cgpt last)`.
    #[arg(long = "continue", short = 'c', default_value_t = false)]
    pub continue_session: bool,

    /// Continue a specific session by id. Mutually exclusive with
    /// `--continue`. The session id is the one shown by `cgpt history`.
    #[arg(long = "resume", value_name = "SESSION_ID", conflicts_with = "continue_session")]
    pub resume: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct HistoryArgs {
    /// Maximum number of past sessions to print. Newest first.
    #[arg(long = "limit", default_value_t = 20)]
    pub limit: usize,
}

#[derive(Debug, clap::Args)]
pub struct ReplayArgs {
    /// Session id to render. Use `cgpt history` to list available ids.
    pub session_id: String,

    /// Disable termimad rendering and print the raw final markdown.
    #[arg(long = "no-pretty", default_value_t = false)]
    pub no_pretty: bool,

    /// After printing, also copy the rendered content to the OS clipboard.
    #[arg(long = "copy", default_value_t = false)]
    pub copy: bool,
}

#[derive(Debug, clap::Args)]
pub struct LastArgs {
    /// Disable termimad rendering and print the raw final markdown.
    #[arg(long = "no-pretty", default_value_t = false)]
    pub no_pretty: bool,

    /// After printing, also copy the rendered content to the OS clipboard.
    #[arg(long = "copy", default_value_t = false)]
    pub copy: bool,
}

pub fn parse() -> Result<Cli, clap::Error> {
    Cli::try_parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn ask_with_inline_prompt() {
        let cli = Cli::try_parse_from(["cgpt", "ask", "hello", "world"]).unwrap();
        match cli.command {
            Command::Ask(a) => {
                assert_eq!(a.prompt, vec!["hello", "world"]);
                assert_eq!(a.timeout_ms, 120_000);
            }
            _ => panic!("expected Ask"),
        }
    }

    #[test]
    fn timeout_override() {
        let cli = Cli::try_parse_from(["cgpt", "ask", "--timeout-ms", "5000", "hi"]).unwrap();
        match cli.command {
            Command::Ask(a) => {
                assert_eq!(a.timeout_ms, 5000);
                assert_eq!(a.prompt, vec!["hi"]);
            }
            _ => panic!("expected Ask"),
        }
    }

    #[test]
    fn socket_override_is_global() {
        let cli = Cli::try_parse_from(["cgpt", "--socket", "/tmp/x.sock", "ask", "hi"]).unwrap();
        assert_eq!(
            cli.socket_override.as_deref().map(|p| p.to_str().unwrap()),
            Some("/tmp/x.sock")
        );
    }

    #[test]
    fn ask_with_no_args_still_parses() {
        // `cgpt ask` alone is valid at the parser level; main.rs decides
        // whether to require stdin or fail with exit 2.
        let cli = Cli::try_parse_from(["cgpt", "ask"]).unwrap();
        match cli.command {
            Command::Ask(a) => assert!(a.prompt.is_empty()),
            _ => panic!("expected Ask"),
        }
    }

    #[test]
    fn unknown_subcommand_is_error() {
        let r = Cli::try_parse_from(["cgpt", "wat"]);
        assert!(r.is_err());
    }

    #[test]
    fn agent_subcommand_parses() {
        let cli = Cli::try_parse_from(["cgpt", "agent", "diagnose", "tests"]).unwrap();
        match cli.command {
            Command::Agent(a) => {
                assert_eq!(a.task, vec!["diagnose", "tests"]);
                assert_eq!(a.timeout_ms, 120_000);
                assert!(!a.buffer);
                assert!(!a.no_pretty);
            }
            _ => panic!("expected Agent"),
        }
    }

    #[test]
    fn ask_buffer_flag_parses_without_prompt() {
        let cli = Cli::try_parse_from(["cgpt", "ask", "--buffer"]).unwrap();
        match cli.command {
            Command::Ask(a) => {
                assert!(a.buffer);
                assert!(a.prompt.is_empty());
            }
            _ => panic!("expected Ask"),
        }
    }

    #[test]
    fn agent_buffer_and_no_pretty_parse() {
        let cli =
            Cli::try_parse_from(["cgpt", "agent", "--buffer", "--no-pretty", "lead"]).unwrap();
        match cli.command {
            Command::Agent(a) => {
                assert!(a.buffer);
                assert!(a.no_pretty);
                assert_eq!(a.task, vec!["lead"]);
            }
            _ => panic!("expected Agent"),
        }
    }

    #[test]
    fn history_subcommand_parses() {
        let cli = Cli::try_parse_from(["cgpt", "history", "--limit", "5"]).unwrap();
        match cli.command {
            Command::History(h) => assert_eq!(h.limit, 5),
            _ => panic!("expected History"),
        }
    }

    #[test]
    fn replay_subcommand_parses() {
        let cli = Cli::try_parse_from(["cgpt", "replay", "s123", "--copy"]).unwrap();
        match cli.command {
            Command::Replay(r) => {
                assert_eq!(r.session_id, "s123");
                assert!(r.copy);
            }
            _ => panic!("expected Replay"),
        }
    }

    #[test]
    fn last_subcommand_parses() {
        let cli = Cli::try_parse_from(["cgpt", "last", "--no-pretty"]).unwrap();
        match cli.command {
            Command::Last(l) => assert!(l.no_pretty),
            _ => panic!("expected Last"),
        }
    }

    #[test]
    fn agent_continue_short_flag() {
        let cli = Cli::try_parse_from(["cgpt", "agent", "-c", "follow-up"]).unwrap();
        match cli.command {
            Command::Agent(a) => {
                assert!(a.continue_session);
                assert!(a.resume.is_none());
            }
            _ => panic!("expected Agent"),
        }
    }

    #[test]
    fn agent_resume_with_id() {
        let cli = Cli::try_parse_from(["cgpt", "agent", "--resume", "abc", "x"]).unwrap();
        match cli.command {
            Command::Agent(a) => assert_eq!(a.resume.as_deref(), Some("abc")),
            _ => panic!("expected Agent"),
        }
    }

    #[test]
    fn continue_and_resume_are_mutually_exclusive() {
        let r =
            Cli::try_parse_from(["cgpt", "agent", "--continue", "--resume", "abc", "x"]);
        assert!(r.is_err());
    }

    #[test]
    fn ask_copy_and_editor_flags() {
        let cli = Cli::try_parse_from(["cgpt", "ask", "--editor", "--copy"]).unwrap();
        match cli.command {
            Command::Ask(a) => {
                assert!(a.editor);
                assert!(a.copy);
            }
            _ => panic!("expected Ask"),
        }
    }
}
