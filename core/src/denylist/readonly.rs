//! Coarse per-stage risk inference.
//!
//! Under `--auto-readonly` a `ReadOnly` command runs without a keypress,
//! after the runner's path check and in-a-row cap. Without that flag it asks
//! like any other class, and `--yolo` runs every class that is not blocked.
//! The gateway uses only the block rules. So the class is conservative by
//! intent, not a guarantee (security.md §3): a
//! stage is `ReadOnly` only if the command is on the allowlist AND the stage
//! has a "plain" shape (no script blocks, no sub-expressions, no call
//! operator, only relative in-project paths). Everything else is `Unknown`
//! or a stronger class.

use super::shell::{
    command_tokens, contains_redirect, has_unquoted, norm_cmd, normalize, tokenize, unwrap_shell,
    Unwrapped,
};
use super::{split_stages, LocalRisk, MAX_UNWRAP_DEPTH};
use regex::Regex;
use std::sync::OnceLock;

const READ_ONLY_CMDS: &[&str] = &[
    // POSIX / cmd
    "ls",
    "cat",
    "head",
    "tail",
    "more",
    "grep",
    "echo",
    "pwd",
    "stat",
    "which",
    "type",
    "findstr",
    "whoami",
    "hostname",
    "ver",
    // PowerShell cmdlets and their aliases
    "get-childitem",
    "dir",
    "gci",
    "get-content",
    "gc",
    "select-string",
    "sls",
    "test-path",
    "get-item",
    "gi",
    "get-itemproperty",
    "gp",
    "get-location",
    "gl",
    "resolve-path",
    "rvpa",
    "split-path",
    "join-path",
    "get-filehash",
    "get-command",
    "gcm",
    "get-help",
    "help",
    "get-date",
    "get-process",
    "gps",
    "ps",
    "get-service",
    "gsv",
    "get-alias",
    "gal",
    "get-member",
    "gm",
    "get-variable",
    "gv",
    "get-psdrive",
    "get-host",
    "get-culture",
    "get-timezone",
    "write-output",
    "write-host",
    "write",
    "out-string",
    "format-table",
    "ft",
    "format-list",
    "fl",
    "format-wide",
    "fw",
    "measure-object",
    "measure",
    "select-object",
    "select",
    "where-object",
    "sort-object",
    "group-object",
    "group",
];

/// Read-only unless a write/exec flag is present (see `unsafe_flag`).
const FLAG_CHECKED_CMDS: &[&str] = &["file", "find", "rg", "sort", "tree", "wc"];

const NETWORK_CMDS: &[&str] = &[
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
    "iwr",
    "irm",
    "invoke-webrequest",
    "invoke-restmethod",
    "test-netconnection",
    "start-bitstransfer",
];

const DESTRUCTIVE_CMDS: &[&str] = &[
    "rm",
    "mv",
    "remove-item",
    "ri",
    "del",
    "erase",
    "rd",
    "rmdir",
    "remove-itemproperty",
    "rp",
];

const WRITE_CMDS: &[&str] = &[
    "set-content",
    "sc",
    "add-content",
    "ac",
    "out-file",
    "tee-object",
    "tee",
    "new-item",
    "ni",
    "copy-item",
    "copy",
    "cp",
    "cpi",
    "move-item",
    "move",
    "mi",
    "rename-item",
    "ren",
    "rni",
    "clear-content",
    "clc",
    "set-item",
    "si",
    "set-itemproperty",
    "sp",
    "new-itemproperty",
    "mkdir",
    "md",
    "expand-archive",
    "compress-archive",
];

/// Tools whose bare version query is read-only.
const VERSION_TOOLS: &[&str] = &[
    "node",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "dotnet",
    "python",
    "python3",
    "py",
    "pip",
    "cargo",
    "rustc",
    "go",
    "java",
    "javac",
    "git",
    "pwsh",
    "powershell",
    "winget",
    "choco",
];

/// `depth` counts the wrappers the stage is inside; past `MAX_UNWRAP_DEPTH`
/// the inner text is not examined and the stage is `Unknown`.
pub(super) fn infer_stage_risk(stage: &str, depth: usize) -> LocalRisk {
    let toks = command_tokens(stage);
    let Some(first_tok) = toks.first() else {
        return LocalRisk::Unknown;
    };

    // `powershell -Command "..."`, `cmd /c "..."`, `bash -c "..."`: judge the
    // inner text.
    let mut risk = match unwrap_shell(stage, &toks) {
        Unwrapped::Inner(_) if depth >= MAX_UNWRAP_DEPTH => LocalRisk::Unknown,
        Unwrapped::Inner(inner) => {
            let inner_stages = split_stages(&normalize(&inner));
            if inner_stages.is_empty() {
                LocalRisk::Unknown
            } else {
                inner_stages.iter().fold(LocalRisk::ReadOnly, |r, s| {
                    max_risk(r, infer_stage_risk(s, depth + 1))
                })
            }
        }
        Unwrapped::Opaque(..) => LocalRisk::Unknown,
        Unwrapped::No => {
            let first = norm_cmd(&first_tok.text);
            let raw: Vec<String> = toks[1..].iter().map(|t| t.text.clone()).collect();
            let lc: Vec<String> = raw.iter().map(|a| a.to_ascii_lowercase()).collect();
            command_risk(&first, &lc, &raw)
        }
    };
    if risk == LocalRisk::ReadOnly && !plain_shape(stage, &first_tok.text) {
        risk = LocalRisk::Unknown;
    }

    // Any redirection (`>`, `>>`) writes; it also lifts `Unknown` to a write.
    if matches!(risk, LocalRisk::ReadOnly | LocalRisk::Unknown) && contains_redirect(stage) {
        risk = LocalRisk::WriteLocal;
    }
    risk
}

fn command_risk(first: &str, lc: &[String], raw: &[String]) -> LocalRisk {
    if first == "git" {
        return git_risk(lc);
    }
    if VERSION_TOOLS.contains(&first) && raw.len() == 1 && is_version_flag(first, &raw[0]) {
        return LocalRisk::ReadOnly;
    }
    if first == "winget" {
        return match lc.first().map(String::as_str) {
            Some("list" | "search") => LocalRisk::ReadOnly,
            _ => LocalRisk::Unknown,
        };
    }
    if first == "find" && lc.iter().any(|a| a == "-delete") {
        return LocalRisk::Destructive;
    }
    if FLAG_CHECKED_CMDS.contains(&first) {
        return if raw.iter().any(|a| unsafe_flag(first, a)) {
            LocalRisk::Unknown
        } else {
            LocalRisk::ReadOnly
        };
    }
    if READ_ONLY_CMDS.contains(&first) {
        return LocalRisk::ReadOnly;
    }
    if NETWORK_CMDS.contains(&first) {
        return LocalRisk::Network;
    }
    // `[IO.Directory]::Delete('C:\', $true)` and friends.
    if DESTRUCTIVE_CMDS.contains(&first)
        || (first.starts_with('[') && (first.contains("::delete") || first.contains("::remove")))
    {
        return LocalRisk::Destructive;
    }
    if WRITE_CMDS.contains(&first) {
        return LocalRisk::WriteLocal;
    }
    LocalRisk::Unknown
}

/// Write or execute flags of otherwise read-only tools, and flags that take
/// the paths to read from a file (only the name of that file is checked).
/// `raw` is the argument as written: `tree -R` and `file -C` differ from
/// `tree -r` and `file -c`.
fn unsafe_flag(cmd: &str, raw: &str) -> bool {
    let arg = raw.to_ascii_lowercase();
    let short = |flags: &str| {
        raw.starts_with('-') && !raw.starts_with("--") && raw[1..].contains(|c| flags.contains(c))
    };
    match cmd {
        "find" => {
            matches!(
                arg.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fls"
            ) || arg.starts_with("-fprint")
                || arg.starts_with("-files0-from")
        }
        // `--pre`, `--pre-glob`, `--hostname-bin` run programs; `-z` runs
        // decompressors.
        "rg" => {
            arg.starts_with("--pre")
                || arg.starts_with("--hostname-bin")
                || arg.starts_with("--search-zip")
                || short("z")
        }
        // `-C` writes `magic.mgc`, `-f` reads names from a file, `-z`/`-Z`
        // may run decompressors. Long forms by their shortest unique prefix.
        "file" => {
            arg.starts_with("--co")
                || arg.starts_with("--f")
                || arg.starts_with("--u")
                || short("CfzZ")
        }
        // `--files0-from` is the only long option starting with `--f`.
        "wc" => arg.starts_with("--f"),
        // sort / tree write with -o / --output (abbreviations, clusters like
        // -ro); `tree -R` writes `00Tree.html` and reruns tree; `tree
        // --fromfile` / `--fromtabfile` print the lines of a file; `sort
        // --files0-from` (`--fil`: `--fie` is `--field-separator`).
        _ => {
            arg.starts_with("--o")
                || arg.starts_with("--compress")
                || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('o'))
                || (cmd == "tree" && (short("R") || arg.starts_with("--from")))
                || (cmd == "sort" && arg.starts_with("--fil"))
        }
    }
}

/// Version flags. `python -v` is verbose mode (starts a REPL), not a version
/// query; for python only `--version` and `-V` count (case-sensitive).
/// A positional `version` is a subcommand only for go, cargo and npm; for
/// node, java, rustc, npx and the like it names a script, class, file or
/// package to run, and `yarn`/`pnpm version` bump the package version.
fn is_version_flag(tool: &str, arg: &str) -> bool {
    if matches!(tool, "python" | "python3" | "py") {
        return arg == "--version" || arg == "-V";
    }
    match arg.to_ascii_lowercase().as_str() {
        "--version" | "-v" | "-version" => true,
        "version" => matches!(tool, "go" | "cargo" | "npm"),
        _ => false,
    }
}

/// `lc` are the lower-cased arguments after `git`.
fn git_risk(lc: &[String]) -> LocalRisk {
    let skip = lc.iter().take_while(|a| *a == "--no-pager").count();
    let args = &lc[skip..];
    let Some(sub) = args.first().map(String::as_str) else {
        return LocalRisk::ReadOnly; // bare `git` prints usage
    };
    let rest = &args[1..];
    match sub {
        "--version" | "version" if args.len() == 1 => LocalRisk::ReadOnly,
        "status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" => {
            // `git diff --output=f` writes a file; git accepts abbreviations
            // such as `--out` (`--o` is ambiguous with `--oneline`).
            if rest.iter().any(|a| {
                let name = a.split('=').next().unwrap_or("");
                name.len() >= 4 && "--output".starts_with(name)
            }) {
                LocalRisk::WriteLocal
            } else {
                LocalRisk::ReadOnly
            }
        }
        "branch" => git_branch_risk(rest),
        "remote" => {
            if rest.is_empty()
                || (rest.len() == 1 && matches!(rest[0].as_str(), "-v" | "--verbose"))
            {
                LocalRisk::ReadOnly
            } else {
                LocalRisk::Unknown
            }
        }
        "push" | "pull" | "fetch" | "clone" | "ls-remote" => LocalRisk::Network,
        "checkout" | "restore" | "reset" | "clean" | "rm" => LocalRisk::Destructive,
        "add" | "commit" | "merge" | "rebase" | "cherry-pick" | "revert" | "stash" | "tag"
        | "mv" | "switch" | "init" | "apply" | "am" => LocalRisk::WriteLocal,
        _ => LocalRisk::Unknown,
    }
}

/// `git branch` lists unless it deletes, renames, copies, re-tracks or
/// creates (a bare positional argument).
fn git_branch_risk(rest: &[String]) -> LocalRisk {
    let mutating = rest.iter().any(|a| {
        if let Some(long) = a.strip_prefix("--") {
            [
                "delete",
                "move",
                "copy",
                "force",
                "set-upstream",
                "unset-upstream",
                "edit-description",
            ]
            .iter()
            .any(|m| long.starts_with(m))
        } else if let Some(short) = a.strip_prefix('-') {
            // `-d`/`-D`, `-m`/`-M`, `-c`/`-C`, `-u`, `-f`, also in clusters.
            short.chars().any(|c| "dmcuf".contains(c))
        } else {
            false
        }
    });
    if mutating {
        return LocalRisk::WriteLocal;
    }
    let filtering = rest.iter().any(|a| {
        matches!(
            a.as_str(),
            "--list"
                | "-l"
                | "--contains"
                | "--no-contains"
                | "--merged"
                | "--no-merged"
                | "--points-at"
        )
    });
    let positional = rest.iter().any(|a| !a.starts_with('-'));
    if positional && !filtering {
        LocalRisk::WriteLocal // `git branch name` creates a branch
    } else {
        LocalRisk::ReadOnly
    }
}

// ---------------------------------------------------------------------------
// "Plain shape" gate for ReadOnly
// ---------------------------------------------------------------------------

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("shape regex compiles"))
}

fn dollar_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\$[a-z_{(?$]")
}
fn percent_var_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"%~?[a-z_][a-z0-9_]*%")
}
fn provider_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?:^|[^a-z0-9_])(?:env|variable|hk(?:lm|cu|cr|u|cc)|cert|function|alias|wsman|registry):",
    )
}
fn drive_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?:^|[^a-z0-9_])[a-z]:")
}
fn cmd_switch_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^/[a-z](?::[a-z0-9]+)?$")
}

/// The stage as a whole must be plain: no script blocks, sub-expressions,
/// call operators, static calls; command by bare name; every argument a
/// relative in-project path (or not a path at all).
fn plain_shape(stage: &str, first: &str) -> bool {
    let lower = stage.to_ascii_lowercase();
    if has_unquoted(stage, |c| c == '{' || c == '&')
        || lower.contains("$(")
        || lower.contains("@(")
        || lower.contains("::")
        || lower.contains("[scriptblock]")
        || lower.contains('`')
    {
        return false;
    }
    let toks = tokenize(stage, false);
    if toks.is_empty() || toks[0].text == "&" || toks[0].text == "." {
        return false;
    }
    // A command given by path may be a planted binary.
    if first.contains('/') || first.contains('\\') {
        return false;
    }
    toks[1..].iter().all(|t| arg_is_safe(&t.text))
}

fn arg_is_safe(arg: &str) -> bool {
    // `a.txt,~\x` is an array of paths: every element must be safe.
    if arg.contains(',') {
        return arg.split(',').all(|item| arg_is_safe(item.trim()));
    }
    let lower = arg.to_ascii_lowercase();
    // `-Path:C:\x`, `--file=/etc/x`: judge the value too. The separators are
    // ASCII, so an index found in `arg` is valid in `arg` itself.
    if arg.starts_with('-') {
        for sep in [':', '='] {
            if let Some(i) = arg.find(sep) {
                if !arg_is_safe(&arg[i + 1..]) {
                    return false;
                }
            }
        }
    }
    if lower.starts_with('~')
        || lower.contains("${")
        || lower.contains("$env:")
        || dollar_re().is_match(&lower)
        || percent_var_re().is_match(&lower)
        || provider_re().is_match(&lower)
        || drive_re().is_match(&lower)
        || lower.starts_with('\\')
        || lower.starts_with("//")
    {
        return false;
    }
    if lower.starts_with('/') && !cmd_switch_re().is_match(&lower) {
        return false;
    }
    if lower.split(['\\', '/', ',']).any(|c| c == "..") {
        return false;
    }
    let segs: Vec<&str> = lower.split(['\\', '/']).collect();
    if segs.len() > 1
        && segs[..segs.len() - 1]
            .iter()
            .any(|s| s.contains(['*', '?', '[']))
    {
        return false;
    }
    true
}

pub(super) fn max_risk(a: LocalRisk, b: LocalRisk) -> LocalRisk {
    let rank = |r: LocalRisk| match r {
        LocalRisk::ReadOnly => 0,
        LocalRisk::Unknown => 1,
        LocalRisk::WriteLocal => 2,
        LocalRisk::Network => 3,
        LocalRisk::Destructive => 4,
        LocalRisk::Blocked => 5,
    };
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}
