//! PowerShell / cmd / POSIX-shell helpers for the denylist: quote-aware
//! tokens, backtick normalisation, shell wrappers (`powershell -Command`,
//! `cmd /c`, `bash -c`, `Start-Process`, `Invoke-Expression "..."`), and the
//! token-based block rules a single regex cannot express.

use super::{apply_block_rules, push_hit, RuleHit, Whole, MAX_UNWRAP_DEPTH};
use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone)]
pub(super) struct Tok {
    /// Byte offset of the first character of the raw token (an opening quote
    /// counts).
    pub start: usize,
    /// Byte offset just after the raw token (a closing quote counts).
    pub end: usize,
    /// Token text with quotes removed.
    pub text: String,
    /// The raw token starts with a quote (`"" `, `"title"`).
    pub quoted: bool,
}

/// Quote-aware split on whitespace (and on `,` when `comma_sep`).
pub(super) fn tokenize(s: &str, comma_sep: bool) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut start = 0usize;
    let mut quoted = false;
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None if c == '"' || c == '\'' => {
                if !has {
                    start = i;
                    quoted = true;
                }
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() || (comma_sep && c == ',') => {
                if has {
                    toks.push(Tok {
                        start,
                        end: i,
                        text: std::mem::take(&mut cur),
                        quoted,
                    });
                    has = false;
                    quoted = false;
                }
            }
            None => {
                if !has {
                    start = i;
                    quoted = false;
                }
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        toks.push(Tok {
            start,
            end: s.len(),
            text: cur,
            quoted,
        });
    }
    toks
}

/// Commands whose switches cmd.exe accepts glued to the name (`rd/s/q`).
const GLUED_SWITCH_CMDS: &[&str] = &[
    "rd", "rmdir", "del", "erase", "dir", "md", "mkdir", "copy", "xcopy", "move", "ren", "rename",
    "type", "more", "attrib", "cmd",
];

/// Tokens of one stage without a leading call operator (`&`, `.`). A call
/// operator glued to a quoted name (`&"reg.exe"`) is dropped too, and cmd
/// switches glued to the first word (`rd/s/q`) become tokens of their own.
pub(super) fn command_tokens(stage: &str) -> Vec<Tok> {
    let mut toks = tokenize(stage, false);
    let skip = toks
        .iter()
        .take_while(|t| t.text == "&" || t.text == ".")
        .count();
    toks.drain(..skip);
    if let Some(first) = toks.first_mut() {
        if first.text.len() > 1
            && first.text.starts_with('&')
            && stage[first.start..].starts_with('&')
        {
            first.text.remove(0);
            first.start += 1;
        }
    }
    split_glued_switches(stage, &mut toks);
    toks
}

fn split_glued_switches(stage: &str, toks: &mut Vec<Tok>) {
    let Some(first) = toks.first() else {
        return;
    };
    // Only an unquoted word: its text is exactly the raw text.
    if !stage[first.start..].starts_with(first.text.as_str()) {
        return;
    }
    let lower = first.text.to_ascii_lowercase();
    let Some(p) = lower.find('/') else {
        return;
    };
    if !GLUED_SWITCH_CMDS.contains(&&lower[..p]) {
        return;
    }
    let segs: Vec<&str> = lower[p + 1..].split('/').collect();
    let is_switch = |s: &&str| {
        let name = s.split(':').next().unwrap_or("");
        name.len() == 1 && name.chars().all(|c| c.is_ascii_alphabetic() || c == '?')
    };
    if !segs.iter().all(is_switch) {
        return;
    }
    let (start, text) = (first.start, first.text.clone());
    let mut out = vec![Tok {
        quoted: false,
        start,
        end: start + p,
        text: text[..p].to_string(),
    }];
    let mut off = p;
    for seg in segs {
        out.push(Tok {
            quoted: false,
            start: start + off,
            end: start + off + 1 + seg.len(),
            text: text[off..off + 1 + seg.len()].to_string(),
        });
        off += 1 + seg.len();
    }
    toks.splice(0..1, out);
}

/// Commands that only run the command after them.
const TRANSPARENT_PREFIXES: &[&str] = &[
    "env", "nohup", "time", "command", "exec", "nice", "setsid", "stdbuf", "busybox", "xargs",
    "wsl",
];
/// Options of a transparent prefix that take the next word as their value.
/// Per prefix, so `env -i make` (where `-i` takes no value) is not read as a
/// bare `env`, and `exec -a sh env` / `nice -n 5 env` skip their value word.
fn prefix_takes_value(prefix: &str, flag: &str) -> bool {
    // Short flags compare case-sensitively (`xargs -L 1` vs `xargs -l`); long
    // flags are folded. Flags whose argument is the command itself (`wsl -e`,
    // `env -S`) or that take only an attached value (`xargs -i`/`-l`/`-e`) are
    // not listed, so the word after them stays the command.
    let short: &[&str] = match prefix {
        "env" => &["-u", "-C"],
        "nice" => &["-n"],
        "exec" => &["-a"],
        "stdbuf" => &["-i", "-o", "-e"],
        "xargs" => &["-I", "-n", "-P", "-d", "-L", "-s", "-E", "-a"],
        "wsl" => &["-d", "-u"],
        _ => &[],
    };
    let long: &[&str] = match prefix {
        "env" => &["--unset", "--chdir"],
        "nice" => &["--adjustment"],
        "stdbuf" => &["--input", "--output", "--error"],
        "xargs" => &[
            "--replace",
            "--max-args",
            "--max-procs",
            "--delimiter",
            "--max-lines",
            "--eof",
            "--arg-file",
            "--process-slot-var",
        ],
        "wsl" => &["--distribution", "--user", "--cd"],
        _ => &[],
    };
    if flag.starts_with("--") {
        long.contains(&flag.to_ascii_lowercase().as_str())
    } else {
        short.contains(&flag)
    }
}

/// A POSIX `NAME=value` word.
fn is_assignment(t: &Tok) -> bool {
    !t.quoted
        && t.text.split_once('=').is_some_and(|(k, _)| {
            k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// `command_tokens` without leading `NAME=value` words and transparent
/// prefixes: `X=1 curl x`, `env A=b bash -c "..."`, `nohup sh x`,
/// `time curl x`, `wsl -d Ubuntu bash`, `xargs sh`. An assignment alone stays.
pub(super) fn exec_tokens(stage: &str) -> Vec<Tok> {
    skip_exec_prefixes(command_tokens(stage))
}

/// `exec_tokens` for tokens already split.
pub(super) fn skip_exec_prefixes(mut toks: Vec<Tok>) -> Vec<Tok> {
    // Walk an index over the prefixes; remove them once.
    let mut at = 0usize;
    loop {
        while at < toks.len() && is_assignment(&toks[at]) {
            // `x=$(curl -s url)` spans words up to its closing parenthesis.
            let mut end = at + 1;
            let depth =
                |t: &Tok| t.text.matches('(').count() as i64 - t.text.matches(')').count() as i64;
            let mut open = depth(&toks[at]);
            while open > 0 && end < toks.len() {
                open += depth(&toks[end]);
                end += 1;
            }
            if end >= toks.len() {
                break; // the assignment alone
            }
            at = end;
        }
        let Some(first) = toks.get(at) else {
            break;
        };
        let name = norm_cmd(&first.text);
        if !TRANSPARENT_PREFIXES.contains(&name.as_str()) {
            break;
        }
        let mut next = at + 1;
        while let Some(t) = toks.get(next) {
            let v = t.text.as_str();
            let assignment = name == "env"
                && !v.starts_with(['-', '$'])
                && v.split_once('=').is_some_and(|(k, _)| {
                    !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_')
                });
            if v.starts_with('-') || assignment {
                let flag = v.split('=').next().unwrap_or("");
                next += if prefix_takes_value(&name, flag) && !v.contains('=') {
                    2
                } else {
                    1
                };
            } else {
                break;
            }
        }
        if next >= toks.len() {
            break; // the prefix alone
        }
        at = next;
    }
    toks.drain(..at);
    toks
}

/// Lower-cased command name: directory and `.exe`/`.cmd`/`.bat`/`.com` removed.
pub(super) fn norm_cmd(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let base = lower.rsplit(['/', '\\']).next().unwrap_or("");
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(b) = base.strip_suffix(ext) {
            return b.to_string();
        }
    }
    base.to_string()
}

/// The stage with its command name normalised: call operator, quotes,
/// directory and `.exe` removed (`& "C:\x\reg.exe" delete` is `reg delete`).
/// `None` when the stage already starts that way.
pub(super) fn canonical_stage(stage: &str) -> Option<String> {
    let toks = command_tokens(stage);
    let first = toks.first()?;
    let name = norm_cmd(&first.text);
    if name.is_empty() {
        return None;
    }
    let canon = format!("{name}{}", stage.get(first.end..)?);
    (canon != stage.trim()).then_some(canon)
}

/// PowerShell escape normalisation outside quotes: a backtick before a
/// letter or digit is dropped (`R`emove-It`em` is `Remove-Item`), a backtick
/// at the end of a line joins the next line.
fn strip_backticks(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == '`' && q == '"' && i + 1 < chars.len() {
                    out.push(c);
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '`' => match chars.get(i + 1) {
                Some(n) if n.is_alphanumeric() => {
                    i += 1;
                    continue;
                }
                Some('\n') => {
                    i += 2;
                    continue;
                }
                Some('\r') => {
                    i += if chars.get(i + 2) == Some(&'\n') {
                        3
                    } else {
                        2
                    };
                    continue;
                }
                _ => {}
            },
            None => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Text as the denylist sees it: PowerShell backtick escapes removed, and the
/// en dash, em dash and horizontal bar that PowerShell accepts as a parameter
/// prefix (`Remove-Item –Recurse`) written as `-`.
pub(super) fn normalize(command: &str) -> String {
    strip_backticks(command).replace(['\u{2013}', '\u{2014}', '\u{2015}'], "-")
}

/// The text as a POSIX shell reads it, for the download rule: a backslash
/// escapes the next character (`\"`, `\'`, `` \` ``, `\$`, `\#` outside
/// quotes; `\"`, `\$`, `` \` `` inside double quotes), an escaped
/// character inside `$'...'` (ANSI-C quoting) too, and those are written
/// `_` so the splitter does not take them for quotes; `$'...'` becomes
/// `'...'`, and a backtick pair becomes `$(...)`. `None` when nothing
/// changed.
pub(super) fn posix_reading(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    // `'`, `"`, or `$` for `$'...'`.
    let mut quote: Option<char> = None;
    let mut in_backticks = false;
    let mut changed = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
            }
            Some('$') => {
                if c == '\\' && next.is_some() {
                    out.push('_');
                    changed = true;
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    quote = None;
                }
            }
            _ => {
                let in_double = quote == Some('"');
                let escapes: &[char] = if in_double {
                    &['"', '$', '`']
                } else {
                    &['"', '\'', '$', '`', '#']
                };
                if c == '\\' {
                    match next {
                        Some(n) if escapes.contains(&n) => {
                            out.push('_');
                            changed = true;
                        }
                        Some(n) => {
                            out.push(c);
                            out.push(n);
                        }
                        None => out.push(c),
                    }
                    i += 2;
                    continue;
                }
                if c == '$' && !in_double && next == Some('\'') {
                    out.push('\'');
                    quote = Some('$');
                    changed = true;
                    i += 2;
                    continue;
                }
                if c == '`' {
                    out.push_str(if in_backticks { ")" } else { "$(" });
                    in_backticks = !in_backticks;
                    changed = true;
                    i += 1;
                    continue;
                }
                if c == '"' {
                    quote = if in_double { None } else { Some('"') };
                } else if c == '\'' && !in_double {
                    quote = Some('\'');
                }
            }
        }
        out.push(c);
        i += 1;
    }
    changed.then_some(out)
}

/// Split a stage on script-block braces (`{`, `}`) outside quotes, keeping
/// `${...}` variable references intact. Braces inside a quoted string belong
/// to that string (`powershell -c "iwr x | % { iex $_ }"` is one part).
pub(super) fn split_script_blocks(stage: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut part_start = 0usize;
    let mut skip_to: Option<usize> = None;
    let mut quote: Option<char> = None;
    let bytes = stage.as_bytes();
    let mut prev_backtick = false;
    for (i, c) in stage.char_indices() {
        if let Some(end) = skip_to {
            if i < end {
                continue;
            }
            skip_to = None;
        }
        if let Some(q) = quote {
            // A backtick-escaped quote stays inside a double-quoted string.
            if c == q && !(q == '"' && prev_backtick) {
                quote = None;
            }
            prev_backtick = c == '`';
            continue;
        }
        prev_backtick = false;
        if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '$' && bytes.get(i + 1) == Some(&b'{') {
            skip_to = Some(stage[i..].find('}').map_or(stage.len(), |p| i + p + 1));
        } else if c == '{' || c == '}' {
            parts.push(&stage[part_start..i]);
            part_start = i + 1;
        }
    }
    parts.push(&stage[part_start..]);
    parts
}

// ---------------------------------------------------------------------------
// Shell wrappers
// ---------------------------------------------------------------------------

pub(super) enum Unwrapped {
    /// Not a wrapper (or nothing to unwrap).
    No,
    /// The text that the wrapper executes.
    Inner(String),
    /// A wrapper whose payload cannot be inspected: (rule name, note).
    Opaque(&'static str, &'static str),
}

const ENCODED: (&str, &str) = (
    "encoded_command",
    "PowerShell -EncodedCommand cannot be inspected and is denylisted.",
);
const OPAQUE_LAUNCH: (&str, &str) = (
    "opaque_shell_launch",
    "Start-Process of a shell with arguments that cannot be inspected is denylisted.",
);

/// One pair of outer matching quotes removed when they wrap the whole text,
/// with the escaped quotes inside unescaped: PowerShell doubles them (`''`,
/// `""`) or escapes them with a backtick, and a command line passes `\"`.
fn strip_outer_quotes(raw: &str) -> Option<String> {
    let q = raw.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let chars: Vec<char> = raw.chars().collect();
    let last = chars.len() - 1;
    let mut out = String::new();
    let mut i = 1;
    while i < chars.len() {
        let c = chars[i];
        if c == q {
            if chars.get(i + 1) == Some(&q) {
                out.push(q);
                i += 2;
                continue;
            }
            return (i == last).then_some(out);
        }
        // A backslash or backtick before the closing quote is just a character.
        if q == '"' && (c == '\\' || c == '`') && chars.get(i + 1) == Some(&'"') && i + 1 < last {
            out.push('"');
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    None
}

fn inner_from_raw(raw: &str) -> Unwrapped {
    let raw = raw.trim();
    let stripped = strip_outer_quotes(raw);
    let inner = stripped.as_deref().unwrap_or(raw).trim();
    if inner.is_empty() {
        Unwrapped::No
    } else {
        Unwrapped::Inner(inner.to_string())
    }
}

/// Recognise commands that run other command text and return that text:
/// `powershell|pwsh [flags] [-Command] <text>`, `cmd /c|/k|/r <text>`,
/// `bash|sh|zsh -c <text>`, `Start-Process <shell> -ArgumentList ...`,
/// `Invoke-Expression "<literal>"`.
pub(super) fn unwrap_shell(stage: &str, toks: &[Tok]) -> Unwrapped {
    let Some(first) = toks.first() else {
        return Unwrapped::No;
    };
    match norm_cmd(&first.text).as_str() {
        "powershell" | "pwsh" => unwrap_powershell(stage, toks),
        "cmd" => unwrap_cmd(stage, toks),
        "bash" | "sh" | "zsh" | "dash" | "ksh" => unwrap_posix_shell(toks),
        "start-process" | "saps" => unwrap_start_process(stage, toks, false),
        "start" => unwrap_start_process(stage, toks, true),
        "invoke-expression" | "iex" => match invoke_expression_arg(toks) {
            Some(t) => match strip_outer_quotes(stage[t.start..].trim()) {
                Some(lit) => inner_from_raw(&lit),
                None => Unwrapped::No,
            },
            None => Unwrapped::No,
        },
        _ => Unwrapped::No,
    }
}

/// The command text of `Invoke-Expression`: first argument, after an optional
/// `-Command`.
fn invoke_expression_arg(toks: &[Tok]) -> Option<&Tok> {
    let name = ps_flag_name(&toks.get(1)?.text);
    if name.is_some_and(|n| n.len() >= 2 && "-command".starts_with(n.as_str())) {
        toks.get(2)
    } else {
        toks.get(1)
    }
}

/// Parameters of powershell/pwsh that consume the next token.
fn ps_flag_takes_value(name: &str) -> bool {
    name == "-ep"
        || [
            "-executionpolicy",
            "-windowstyle",
            "-inputformat",
            "-outputformat",
            "-version",
            "-configurationname",
            "-workingdirectory",
            "-settingsfile",
            "-psconsolefile",
        ]
        .iter()
        .any(|f| name.len() >= 2 && f.starts_with(name))
}

/// Lower-cased name of a powershell/pwsh parameter, with `-` prefix and
/// without a `:value` part. powershell.exe also takes `/enc` for `-enc`.
fn ps_flag_name(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let name = lower.split(':').next().unwrap_or("");
    if name.len() >= 2 && name.starts_with('-') {
        return Some(name.to_string());
    }
    if name.len() >= 2
        && name.starts_with('/')
        && name[1..].chars().all(|c| c.is_ascii_alphabetic())
    {
        return Some(format!("-{}", &name[1..]));
    }
    None
}

fn unwrap_powershell(stage: &str, toks: &[Tok]) -> Unwrapped {
    let mut i = 1;
    while i < toks.len() {
        let t = &toks[i];
        // `powershell "-enc AAAA"`: a quoted word holding switches.
        if t.text.contains(char::is_whitespace)
            && t.text
                .split_whitespace()
                .next()
                .is_some_and(|w| ps_flag_name(w).is_some())
        {
            let synthetic = format!("powershell {}", t.text);
            return unwrap_powershell(&synthetic, &command_tokens(&synthetic));
        }
        if let Some(name) = ps_flag_name(&t.text) {
            let name = name.as_str();
            if name == "-e"
                || name == "-ec"
                || (name.len() >= 3 && "-encodedcommand".starts_with(name))
            {
                return Unwrapped::Opaque(ENCODED.0, ENCODED.1);
            }
            if name.len() >= 2 && "-command".starts_with(name) {
                if let Some((_, v)) = t.text.split_once(':') {
                    return inner_from_raw(v);
                }
                return match toks.get(i + 1) {
                    Some(n) => inner_from_raw(&stage[n.start..]),
                    None => Unwrapped::No,
                };
            }
            if name.len() >= 2 && "-file".starts_with(name) {
                return Unwrapped::No;
            }
            i += if ps_flag_takes_value(name) && !t.text.contains(':') {
                2
            } else {
                1
            };
            continue;
        }
        // First positional argument: PowerShell treats it (and the rest) as
        // the command text.
        return inner_from_raw(&stage[t.start..]);
    }
    Unwrapped::No
}

/// `cmd /c|/k|/r <text>`; switches may be glued (`/d/c`, `/crd ...`) and the
/// text after the switch is the rest of the line.
fn unwrap_cmd(stage: &str, toks: &[Tok]) -> Unwrapped {
    for t in toks.iter().skip(1) {
        if !t.text.starts_with('/') {
            return Unwrapped::No;
        }
        // Byte offset of the first character of each `/`-separated switch.
        let mut pos = 1usize;
        for seg in t.text[1..].split('/') {
            if matches!(
                seg.chars().next().map(|c| c.to_ascii_lowercase()),
                Some('c' | 'k' | 'r')
            ) {
                let glued = &t.text[pos + 1..];
                let rest = stage.get(t.end..).unwrap_or("");
                return if glued.is_empty() {
                    inner_from_raw(rest)
                } else {
                    // `/crd ...`: the rest of the token, then of the line.
                    inner_from_raw(&format!("{glued} {rest}"))
                };
            }
            pos += seg.len() + 1;
        }
    }
    Unwrapped::No
}

fn unwrap_posix_shell(toks: &[Tok]) -> Unwrapped {
    for (i, t) in toks.iter().enumerate().skip(1) {
        let lower = t.text.to_ascii_lowercase();
        if lower.starts_with('-') && !lower.starts_with("--") && lower.ends_with('c') {
            return match toks.get(i + 1) {
                Some(n) => inner_from_raw(&n.text),
                None => Unwrapped::No,
            };
        }
    }
    Unwrapped::No
}

/// Elements of `-ArgumentList a,'b c',d` starting at `raw`.
fn parse_argument_list(raw: &str) -> Vec<String> {
    let mut elems = Vec::new();
    let mut rest = raw.trim_start();
    loop {
        let (text, after) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => match rest[1..].find(q) {
                Some(p) => (rest[1..1 + p].to_string(), &rest[p + 2..]),
                None => (rest[1..].to_string(), ""),
            },
            Some(_) => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == ',')
                    .unwrap_or(rest.len());
                (rest[..end].to_string(), &rest[end..])
            }
            None => break,
        };
        elems.push(text);
        let after = after.trim_start();
        match after.strip_prefix(',') {
            Some(next) => rest = next.trim_start(),
            None => break,
        }
    }
    elems
}

fn unwrap_start_process(stage: &str, toks: &[Tok], cmd_start: bool) -> Unwrapped {
    let is_shell = |t: &Tok| {
        matches!(
            norm_cmd(&t.text).as_str(),
            "powershell" | "pwsh" | "cmd" | "bash" | "sh" | "zsh" | "wsl"
        )
    };
    let mut prog: Option<usize> = None;
    let mut args_at: Option<usize> = None;
    let mut positionals = 0;
    let mut i = 1;
    while i < toks.len() {
        let lower = toks[i].text.to_ascii_lowercase();
        // A word with a space in it (`'-enc AAAA'`) is an argument, not a switch.
        let is_switch = !lower.contains(char::is_whitespace)
            && (lower.starts_with('-') || (cmd_start && lower.starts_with('/')));
        if is_switch {
            let name = lower.split(':').next().unwrap_or("");
            if name == "-args" || (name.len() >= 2 && "-argumentlist".starts_with(name)) {
                args_at = Some(i + 1);
                break;
            }
            if name.len() >= 3 && "-filepath".starts_with(name) {
                prog = Some(i + 1);
                i += 2;
                continue;
            }
            let takes_value = [
                "-workingdirectory",
                "-verb",
                "-windowstyle",
                "-redirectstandarderror",
                "-redirectstandardinput",
                "-redirectstandardoutput",
                "-credential",
            ]
            .iter()
            .any(|f| name.len() >= 3 && f.starts_with(name));
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        positionals += 1;
        match positionals {
            1 if prog.is_none() => prog = Some(i),
            _ => {
                if args_at.is_none() && !cmd_start {
                    args_at = Some(i);
                    break;
                }
            }
        }
        if cmd_start {
            break;
        }
        i += 1;
    }
    let Some(p) = prog.filter(|p| *p < toks.len()) else {
        return Unwrapped::No;
    };
    if !is_shell(&toks[p]) {
        return Unwrapped::No;
    }
    // `start powershell -Command ...` (cmd style): the rest is the command line.
    let synthetic = if cmd_start && args_at.is_none() {
        stage[toks[p].start..].to_string()
    } else if let Some(a) = args_at.filter(|a| *a < toks.len()) {
        let elems = parse_argument_list(&stage[toks[a].start..]);
        let args = if elems.len() == 1 {
            elems[0].clone()
        } else {
            elems
                .iter()
                .map(|e| format!("\"{e}\""))
                .collect::<Vec<_>>()
                .join(" ")
        };
        format!("{} {}", toks[p].text, args)
    } else {
        return Unwrapped::No; // `Start-Process powershell` alone
    };
    let syn_toks = command_tokens(&synthetic);
    match unwrap_shell(&synthetic, &syn_toks) {
        Unwrapped::No => Unwrapped::Opaque(OPAQUE_LAUNCH.0, OPAQUE_LAUNCH.1),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Recursive delete of a disk root or profile
// ---------------------------------------------------------------------------

/// `-Recurse` (or any unambiguous prefix), `-rf`-style clusters, GNU
/// `--recursive` (any prefix down to `--r`, also `--recursive=...`), cmd `/s`
/// (also joined: `/s/q`).
fn is_recurse_flag(lower: &str) -> bool {
    if let Some(sw) = lower.strip_prefix('/') {
        let parts: Vec<&str> = sw.split('/').collect();
        return parts.iter().all(|p| p.len() == 1) && parts.contains(&"s");
    }
    if let Some(long) = lower.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or("");
        return !name.is_empty() && "recursive".starts_with(name);
    }
    let name = lower.split(':').next().unwrap_or("");
    if !name.starts_with('-') || name.len() < 2 {
        return false;
    }
    "-recurse".starts_with(name)
        || (name.len() <= 5
            && name[1..].chars().all(|c| "rfiv".contains(c))
            && name[1..].contains('r'))
}

pub(super) fn root_target_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // `$env:X`, `${env:X}` and `%X%` for the variables that name a disk
        // root or a profile; two may be glued (`$env:HOMEDRIVE$env:HOMEPATH`).
        let var = ["userprofile", "systemroot", "windir", "systemdrive", "homedrive", "homepath"]
            .iter()
            .map(|v| format!(r"\$env:{v}|\$\{{env:{v}\}}|%{v}%"))
            .collect::<Vec<_>>()
            .join("|");
        Regex::new(&format!(
            r"^(?:[a-z]:|[a-z]:\\windows|[a-z]:\\users|[a-z]:\\users\\[^\\]+|~|\$home|\$\{{home\}}|(?:{var})(?:{var})?)$"
        ))
        .expect("root target regex compiles")
    })
}

/// Is this path (lower-cased) the root of a disk, a user profile, or the
/// Windows folder? Understands `\`/`/`, trailing wildcards, `.` and `..`
/// components and the `\\?\` prefix. Going above an anchor with `..` counts.
fn path_is_root(lower: &str) -> bool {
    let mut s = lower
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .replace('/', "\\");
    for prefix in ["\\\\?\\", "\\\\.\\"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    let rooted = s.starts_with('\\');
    let mut comps: Vec<&str> = Vec::new();
    for c in s.split('\\') {
        match c {
            "" | "." => {}
            ".." => {
                if comps.len() == 1 && root_target_re().is_match(comps[0]) {
                    return true;
                }
                comps.pop();
            }
            _ => comps.push(c),
        }
    }
    if comps
        .last()
        .is_some_and(|l| l.chars().all(|c| matches!(c, '*' | '?' | '.')))
    {
        comps.pop();
    }
    if comps.is_empty() {
        // `\`, `/*` are the root; a bare `*` is the current folder.
        return rooted;
    }
    root_target_re().is_match(&comps.join("\\"))
}

fn is_root_target(arg: &str) -> bool {
    // `-Path @('C:\','D:\')`, `(C:\)`: look at every comma-separated item.
    arg.split(',').any(|item| {
        let lower = item
            .trim()
            .trim_matches(|c| matches!(c, '(' | ')' | '@' | '\'' | '"'))
            .to_ascii_lowercase();
        if let Some(flag) = lower.strip_prefix('-') {
            // `-Path:C:\` carries its value after the first colon.
            return flag.split_once(':').is_some_and(|(_, v)| path_is_root(v));
        }
        // cmd switches: `/q`, `/s/q`.
        if lower.len() > 1 && lower.starts_with('/') && lower[1..].split('/').all(|p| p.len() == 1)
        {
            return false;
        }
        path_is_root(&lower)
    })
}

/// Recursive delete (PowerShell, cmd, or POSIX-style `rm -rf` flags) of a
/// disk root or profile. Recursive delete of a project subfolder is not this.
fn is_recursive_root_delete(toks: &[Tok]) -> bool {
    let first = norm_cmd(&toks[0].text);
    if !matches!(
        first.as_str(),
        "remove-item" | "ri" | "rm" | "del" | "erase" | "rd" | "rmdir"
    ) {
        return false;
    }
    let args = &toks[1..];
    args.iter()
        .any(|t| is_recurse_flag(&t.text.to_ascii_lowercase()))
        && args.iter().any(|t| is_root_target(&t.text))
}

// ---------------------------------------------------------------------------
// Block rules that need tokens
// ---------------------------------------------------------------------------

/// Token-based Windows rules plus looking inside shell wrappers. Script
/// blocks (`{ ... }`) are examined part by part; a lone string literal as a
/// statement (the argument of a parenthesised `iex ("...")`) is examined as
/// command text.
pub(super) fn apply_windows_block_rules(
    stage: &str,
    hits: &mut Vec<RuleHit>,
    depth: usize,
    whole: &Whole,
) {
    for part in split_script_blocks(stage) {
        let toks = exec_tokens(part);
        if toks.is_empty() {
            continue;
        }
        if is_recursive_root_delete(&toks) {
            push_hit(
                hits,
                RuleHit {
                    rule_name: "recursive_delete_root_or_profile",
                    stage: stage.to_string(),
                    note: "recursive deletion of a disk root or user profile.".to_string(),
                },
            );
        }
        match unwrap_shell(part, &toks) {
            Unwrapped::Inner(inner) if depth < MAX_UNWRAP_DEPTH => {
                apply_block_rules(&normalize(&inner), hits, depth + 1, whole)
            }
            Unwrapped::Inner(_) => {}
            Unwrapped::Opaque(rule_name, note) => push_hit(
                hits,
                RuleHit {
                    rule_name,
                    stage: stage.to_string(),
                    note: note.to_string(),
                },
            ),
            Unwrapped::No => {}
        }
        if depth < MAX_UNWRAP_DEPTH && toks.len() == 1 && strip_outer_quotes(part.trim()).is_some()
        {
            apply_block_rules(&normalize(&toks[0].text), hits, depth + 1, whole);
        }
    }
}

/// Text of every `$(...)`, `@(...)` and backtick pair that the shell would
/// run when it expands the string, in double quotes or bare. Single quotes
/// are literal. Nested ones are found when the returned text is scanned in
/// turn; an unclosed `$(` runs to the end.
pub(super) fn subexpressions(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let (mut in_single, mut in_double) = (false, false);
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            // PowerShell escape inside a double-quoted string.
            '`' if in_double && chars.get(i + 1) == Some(&'"') => {
                i += 2;
                continue;
            }
            '$' | '@' if !in_single && chars.get(i + 1) == Some(&'(') => {
                let mut depth = 1usize;
                let mut j = i + 2;
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                let end = if depth == 0 { j - 1 } else { j };
                out.push(chars[i + 2..end].iter().collect());
                i = j;
                continue;
            }
            '`' if in_double => {
                if let Some(close) = chars[i + 1..].iter().position(|c| *c == '`') {
                    out.push(chars[i + 1..i + 1 + close].iter().collect());
                    i += close + 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Shape helpers for the read-only classifier
// ---------------------------------------------------------------------------

/// Does `pred` hold for a character outside quotes?
pub(super) fn has_unquoted(stage: &str, pred: impl Fn(char) -> bool) -> bool {
    let mut quote: Option<char> = None;
    for c in stage.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None if pred(c) => return true,
            None => {}
        }
    }
    false
}

/// A backslash outside single quotes right before a quote, a stage
/// separator (`;`, `|`, `&`), a redirect (`<`, `>`) or a newline. The shell
/// reads it as an escape; `split_stages`, `tokenize` and `has_unquoted` do
/// not, so the stages and words they see may not be the shell's (Ruling 49).
pub(super) fn escapes_an_operator(command: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => match chars.next() {
                Some(n) if "'\";|&<>\n\r".contains(n) => return true,
                _ => {}
            },
            (Some(_), '"') => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            _ => {}
        }
    }
    false
}

/// A `>` outside quotes: `>`, `>>`, `2>`, `2>&1`.
pub(super) fn contains_redirect(stage: &str) -> bool {
    has_unquoted(stage, |c| c == '>')
}
