//! The narrow list of `cgpt agent --auto-readonly` (P1, research
//! 2026-10-27-auto-readonly-narrow-list).
//!
//! A command runs without a keypress only if the classifier calls it
//! `ReadOnly` AND it passes this list; one never replaces the other, and the
//! panel shows the classifier's class as before. The list holds the commands
//! that only read the project's files, each with a whitelist of flags, so a
//! flag that follows links, reads paths from a file, writes or runs a program
//! asks. The runner then checks every path and where each program comes from.
//!
//! The command is read as written, without the classifier's normalisation:
//! - a backslash outside single quotes asks: the splitter and tokenizer
//!   shared with the classifier do not read backslash escapes, the shell
//!   does, so past one the stages and words seen here may not be the
//!   shell's (Ruling 49);
//! - every stage (`|`, `;`, `&&`, `||`, newline) must pass;
//! - the program is the raw first word, compared exactly: no path, no other
//!   case or extension, no wrapper (`env`, `xargs`, `bash -c`, `NAME=value`),
//!   no leading `.` or `&`;
//! - flags are compared with their case; short ones are read one by one
//!   inside a cluster, long ones only by their full name (GNU takes any
//!   unambiguous abbreviation, the list does not);
//! - GNU tools take flags after the operands, so every word up to `--` is
//!   checked; after `--` come only operands;
//! - a stage with `<`, `>` or `&` outside quotes asks;
//! - a word naming a known secrets location (`names_secret_location`) asks.
//!
//! `echo` takes any arguments only because no command on the list reads the
//! paths to open from stdin: `--files0-from=-` and `file -f -` are off it.

use super::shell::{has_unquoted, tokenize, Tok};
use super::{names_secret_location, split_stages};

/// Whether a long flag takes a value.
#[derive(Clone, Copy)]
enum Arg {
    /// `--flag` only.
    No,
    /// `--flag=value` or `--flag value`.
    Required,
    /// `--flag` or `--flag=value`.
    Optional,
}

/// How a command on the list reads its arguments.
enum Syntax {
    /// getopt-style flags.
    Flags {
        /// Short flags without a value.
        short: &'static str,
        /// Short flags that take a value.
        short_value: &'static str,
        /// A value may follow its short flag in the same word (`-n20`,
        /// `-A3`). tree's own parser takes it from the next word and goes on
        /// reading the cluster, so for tree a value flag must end its word.
        glued_values: bool,
        long: &'static [(&'static str, Arg)],
    },
    /// find: start options, paths and an expression of listed primaries.
    Find,
    /// Any arguments (`echo`).
    Any,
}

use Arg::{No, Optional, Required};

const fn flags(
    short: &'static str,
    short_value: &'static str,
    long: &'static [(&'static str, Arg)],
) -> Syntax {
    Syntax::Flags {
        short,
        short_value,
        glued_values: true,
        long,
    }
}

/// The list and its flags (research card, «Команды и флаги», with the
/// user's decisions of 28.10: no git, `which` with names only, no rg into
/// hidden or ignored files).
const LIST: &[(&str, Syntax)] = &[
    ("ls", flags("1aAlhRtSrdFpinG", "", &[("--color", Optional)])),
    ("cat", flags("nbsvetAET", "", &[])),
    ("head", flags("qv", "nc", &[])),
    ("tail", flags("qvr", "nc", &[])),
    ("wc", flags("lwcmL", "", &[])),
    (
        "stat",
        // BSD `-f`/`-t` take a format, GNU's do not: read as taking none,
        // the format word is then checked as an operand.
        flags(
            "Lftxsln",
            "c",
            &[
                ("--format", Required),
                ("--printf", Required),
                ("--terse", No),
            ],
        ),
    ),
    (
        "file",
        flags(
            "biILhk",
            "",
            &[("--mime-type", No), ("--mime-encoding", No)],
        ),
    ),
    (
        "grep",
        flags(
            "rniwxclLhHovEFGPsqIOp",
            "efmABC",
            &[
                ("--include", Required),
                ("--exclude", Required),
                ("--exclude-dir", Required),
                ("--color", Optional),
            ],
        ),
    ),
    (
        "rg",
        flags(
            "isSwxnNlcFo",
            "egtTABCmd",
            &[
                ("--max-depth", Required),
                ("--files", No),
                ("--count", No),
                ("--json", No),
                ("--vimgrep", No),
                ("--heading", No),
                ("--no-heading", No),
                ("--sort", Required),
                ("--no-config", No),
            ],
        ),
    ),
    ("find", Syntax::Find),
    (
        "tree",
        Syntax::Flags {
            short: "adfishDFCnpugJX",
            short_value: "LIP",
            glued_values: false,
            long: &[
                ("--gitignore", No),
                ("--noreport", No),
                ("--dirsfirst", No),
                ("--charset", Required),
            ],
        },
    ),
    ("sort", flags("nrufhVbsMgcCR", "kt", &[])),
    ("echo", Syntax::Any),
    ("pwd", flags("LP", "", &[])),
    // Names only: in zsh `which` prints the user's function definitions.
    ("which", flags("", "", &[])),
];

/// find's start options and primaries without a value.
const FIND_NO_VALUE: &[&str] = &[
    "-H", "-P", "-empty", "-prune", "-print", "-print0", "-ls", "-not", "!", "-a", "-o", "-and",
    "-or", "(", ")",
];
/// find's primaries that take the next word.
const FIND_ONE_VALUE: &[&str] = &[
    "-name",
    "-iname",
    "-path",
    "-ipath",
    "-regex",
    "-iregex",
    "-type",
    "-maxdepth",
    "-mindepth",
    "-size",
    "-mtime",
    "-mmin",
    "-newer",
    "-printf",
];

/// The programs `command` runs (the first word of each stage) when
/// `--auto-readonly` may run it without a keypress, or why it must ask, in
/// one line. The caller still checks the paths and where each program is
/// found.
pub fn auto_run_programs(command: &str) -> Result<Vec<&'static str>, String> {
    if backslash_outside_single_quotes(command) {
        return Err("a backslash escape is not on the auto-run list".to_string());
    }
    let stages = split_stages(command);
    if stages.is_empty() {
        return Err("an empty command is not on the auto-run list".to_string());
    }
    let mut programs = Vec::new();
    for stage in &stages {
        programs.push(check_stage(stage)?);
    }
    Ok(programs)
}

/// Up to the first backslash nothing is escaped, so the quote state is the
/// shell's: a backslash counts unless it is inside `'...'`.
fn backslash_outside_single_quotes(command: &str) -> bool {
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => return true,
            (Some(_), '"') => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            _ => {}
        }
    }
    false
}

fn check_stage(stage: &str) -> Result<&'static str, String> {
    let toks = tokenize(stage, false);
    let Some(first) = toks.first() else {
        return Err("an empty command is not on the auto-run list".to_string());
    };
    let raw = &stage[first.start..first.end];
    let Some((name, syntax)) = LIST.iter().find(|(name, _)| *name == raw) else {
        return Err(format!("`{}` is not on the auto-run list", raw));
    };
    if has_unquoted(stage, |c| matches!(c, '<' | '>' | '&')) {
        return Err(format!(
            "`<`, `>` or `&` after `{}` is not on the auto-run list",
            name
        ));
    }
    let args = &toks[1..];
    match syntax {
        Syntax::Flags {
            short,
            short_value,
            glued_values,
            long,
        } => check_flags(name, args, short, short_value, *glued_values, long)?,
        Syntax::Find => check_find(args)?,
        Syntax::Any => {}
    }
    if names_secret_location(stage) {
        return Err(format!(
            "`{}` names a known secrets location (.env, keys, credentials, .git)",
            stage
        ));
    }
    Ok(name)
}

fn off_list(flag: &str, name: &str) -> String {
    format!("flag `{}` of `{}` is not on the auto-run list", flag, name)
}

fn check_flags(
    name: &str,
    args: &[Tok],
    short: &str,
    short_value: &str,
    glued_values: bool,
    long: &[(&str, Arg)],
) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let word = args[i].text.as_str();
        i += 1;
        if word == "--" {
            return Ok(());
        }
        if word == "-" || !word.starts_with('-') {
            continue; // an operand
        }
        if let Some(body) = word.strip_prefix("--") {
            let (flag, value) = match body.split_once('=') {
                Some((f, _)) => (&word[..2 + f.len()], true),
                None => (word, false),
            };
            match long.iter().find(|(l, _)| *l == flag).map(|(_, arg)| *arg) {
                Some(Arg::No) if value => return Err(off_list(word, name)),
                Some(Arg::Required) if !value => i += 1,
                Some(_) => {}
                None => return Err(off_list(flag, name)),
            }
            continue;
        }
        for (at, c) in word[1..].char_indices() {
            if short.contains(c) {
                continue;
            }
            if !short_value.contains(c) {
                return Err(off_list(&format!("-{}", c), name));
            }
            let rest = &word[1 + at + c.len_utf8()..];
            if rest.is_empty() {
                i += 1; // the value is the next word
            } else if !glued_values {
                return Err(off_list(word, name));
            }
            break;
        }
    }
    Ok(())
}

fn check_find(args: &[Tok]) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let word = args[i].text.as_str();
        i += 1;
        if FIND_ONE_VALUE.contains(&word) {
            i += 1;
        } else if !FIND_NO_VALUE.contains(&word) && word.starts_with('-') {
            return Err(off_list(word, "find"));
        }
    }
    Ok(())
}
