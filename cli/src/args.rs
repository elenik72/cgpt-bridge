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

    /// Disable rendering the final assistant message through `glow`. By
    /// default, when stdout is a TTY and `glow` is on PATH, the final
    /// markdown is pretty-printed. With this flag the raw markdown is
    /// emitted verbatim. Note: piped/redirected stdout already disables
    /// rendering automatically.
    #[arg(long = "no-pretty", default_value_t = false)]
    pub no_pretty: bool,
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
            Command::Agent(_) => panic!("expected Ask"),
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
            Command::Agent(_) => panic!("expected Ask"),
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
            Command::Agent(_) => panic!("expected Ask"),
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
            Command::Ask(_) => panic!("expected Agent"),
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
            Command::Agent(_) => panic!("expected Ask"),
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
            Command::Ask(_) => panic!("expected Agent"),
        }
    }
}
