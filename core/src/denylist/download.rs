//! Download followed by execution, judged by command tokens.
//!
//! A command line that downloads something and then runs it is denylisted:
//! `iwr x | iex`, `$s = iwr x; iex $s`, `curl x | sh`,
//! `iwr x -OutFile a.ps1; .\a.ps1`. Running is decided by the command word of
//! a stage, never by a substring: `curl https://api.iex.cloud/v1/data` and
//! `dotnet run & curl localhost:5000` are ordinary commands. A single `&` is
//! a POSIX background operator here, not PowerShell's call operator.
//!
//! What counts as "the download" is what the command writes: `-o`/`-OutFile`
//! targets, `-O` (name from the URL), redirections, `Out-File`/`tee`, a
//! variable that receives it. Words of the URL are not names of files.
//! Wrappers (`powershell -c "..."`, `cmd /c`, `bash -c`) and `$(...)` are
//! looked into, to the same depth as the other rules.

use super::shell::{
    exec_tokens, norm_cmd, normalize, split_script_blocks, subexpressions, tokenize, unwrap_shell,
    Tok, Unwrapped,
};
use super::{push_hit, split_stages_piped, RuleHit, MAX_UNWRAP_DEPTH};
use std::collections::HashSet;

const DOWNLOADERS: &[&str] = &[
    "iwr",
    "irm",
    "invoke-webrequest",
    "invoke-restmethod",
    "curl",
    "wget",
    "start-bitstransfer",
];
const POSIX_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];
const WIN_SHELLS: &[&str] = &["powershell", "pwsh", "cmd", "wsl"];
/// Programs that load a file given as an argument.
const FILE_RUNNERS: &[&str] = &[
    "msiexec", "wscript", "cscript", "mshta", "rundll32", "regsvr32",
];
/// Commands that run text or a file.
const LAUNCHERS: &[&str] = &[
    "invoke-command",
    "icm",
    "start-process",
    "saps",
    "start",
    "invoke-item",
    "ii",
    "import-module",
    "ipmo",
];
/// Interpreters that run a script given by name, or standard input.
const INTERPRETERS: &[&str] = &["python", "python3", "py", "node", "perl", "ruby", "php"];
/// Options of shells that take a value, so the value is not the script.
const VALUE_FLAGS: &[&str] = &[
    "-executionpolicy",
    "-ep",
    "-windowstyle",
    "-w",
    "-version",
    "-workingdirectory",
    "-wd",
    "-configurationname",
    "-inputformat",
    "-outputformat",
    "-settingsfile",
    "-verb",
    "-o",
    "--rcfile",
    "--init-file",
];
/// Commands that write their arguments' files.
const WRITERS: &[&str] = &[
    "out-file",
    "set-content",
    "add-content",
    "tee",
    "tee-object",
    "sc",
    "ac",
];
const EXEC_EXTS: &[&str] = &[
    ".exe", ".ps1", ".bat", ".cmd", ".com", ".msi", ".vbs", ".sh", ".scr",
];

/// Everything one stage tells us.
struct StageInfo {
    /// The stage reads the output of the previous one.
    piped: bool,
    segments: Vec<Segment>,
    /// The stage itself downloads: a downloader word, `$(curl ...)`.
    own_downloads: bool,
    /// ... or a wrapper in it downloads (`powershell -c "iwr x"`).
    downloads: bool,
    /// `<(curl ...)` feeds a shell or `source`.
    feeds_shell: bool,
    /// `[scriptblock]::Create(...)`.
    creates_scriptblock: bool,
    /// Names the download is written to (and variables that receive it).
    targets: HashSet<String>,
    /// Files written by redirections and `Out-File`/`tee`.
    writes: HashSet<String>,
    /// `$(...)` texts of the stage.
    subs: Vec<Vec<StageInfo>>,
}

/// One command of a stage: script blocks (`{ ... }`) split a stage.
struct Segment {
    /// First token as written (to see the call operator or dot-sourcing).
    raw_first: String,
    toks: Vec<Tok>,
    /// The text a shell wrapper runs (`powershell -c "..."`), analysed.
    inner: Vec<StageInfo>,
    /// The inline code of a shell or interpreter, or the here-string it
    /// reads as its script, holds a download that the calling shell expands
    /// into it: `bash -c "...$(curl x)"`, `python3 -c "$(curl x)"`,
    /// `bash <<< "$(curl x)"`.
    code_downloads: bool,
    /// The arguments as the command sees them: redirections and their
    /// targets left out, nothing after a `#` comment word.
    args: Vec<Tok>,
    /// Where a `<` redirection reads standard input from (`< a.sh`,
    /// `< <(curl x)` gives `<(curl`).
    stdin_from: Option<String>,
}

pub(super) fn apply(text: &str, hits: &mut Vec<RuleHit>) {
    apply_as(text, text, hits);
}

/// The rule on `text`, a hit showing `shown`: the classifier also passes the
/// command as a POSIX shell reads it (`shell::posix_reading`), since the
/// splitter lets a backslash escape nothing, as in PowerShell, and
/// `echo \" ; curl x | sh ; echo \"` runs `curl x | sh`.
pub(super) fn apply_as(text: &str, shown: &str, hits: &mut Vec<RuleHit>) {
    scan(&analyze(text, 0), shown, hits);
}

fn scan(stages: &[StageInfo], text: &str, hits: &mut Vec<RuleHit>) {
    if let Some(posix) = detect(stages, false, &HashSet::new(), 0) {
        push_hit(hits, hit(text, posix));
    }
    for info in stages {
        for seg in &info.segments {
            scan(&seg.inner, text, hits);
        }
        for sub in &info.subs {
            scan(sub, text, hits);
        }
    }
}

fn hit(text: &str, posix_shell: bool) -> RuleHit {
    if posix_shell {
        RuleHit {
            rule_name: "fetch_then_exec",
            stage: text.to_string(),
            note: "downloading and piping into a shell is denylisted.".to_string(),
        }
    } else {
        RuleHit {
            rule_name: "download_and_execute",
            stage: text.to_string(),
            note: "downloading and executing code in one command line is denylisted.".to_string(),
        }
    }
}

fn analyze(text: &str, depth: usize) -> Vec<StageInfo> {
    split_stages_piped(text, false)
        .iter()
        .map(|(stage, piped)| stage_info(without_background(stage), *piped, depth))
        .collect()
}

/// The stage without a trailing background `&` (`sh &`, `sh 2>&1&`), which
/// the splitter leaves on it; `2>&` or `&&` are not one.
fn without_background(stage: &str) -> &str {
    let trimmed = stage.trim_end();
    match trimmed.strip_suffix('&') {
        Some(rest) if !rest.ends_with(['&', '>', '<', '|']) => rest.trim_end(),
        _ => stage,
    }
}

fn stage_info(stage: &str, piped: bool, depth: usize) -> StageInfo {
    let lower = stage.to_ascii_lowercase();
    let squeezed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    let segments: Vec<Segment> = split_script_blocks(stage)
        .into_iter()
        .filter_map(|part| {
            let toks = exec_tokens(part);
            let raw_first = tokenize(part, false).into_iter().next()?.text;
            if toks.is_empty() {
                return None;
            }
            let inner = match unwrap_shell(part, &toks) {
                Unwrapped::Inner(text) if depth < MAX_UNWRAP_DEPTH => {
                    analyze(&normalize(&text), depth + 1)
                }
                _ => Vec::new(),
            };
            let code_downloads =
                depth < MAX_UNWRAP_DEPTH && inline_code_downloads(part, &toks, depth);
            let (args, stdin_from) = command_args(&toks[1..]);
            Some(Segment {
                raw_first,
                toks,
                inner,
                code_downloads,
                args,
                stdin_from,
            })
        })
        .collect();
    let subs: Vec<Vec<StageInfo>> = if depth < MAX_UNWRAP_DEPTH {
        subexpressions(stage)
            .iter()
            .map(|sub| analyze(sub, depth + 1))
            .collect()
    } else {
        Vec::new()
    };
    let own_downloads = segments.iter().any(has_download)
        || [".downloadstring", ".downloadfile", ".downloaddata"]
            .iter()
            .any(|m| lower.contains(m))
        || subs.iter().flatten().any(|s| s.downloads);
    let downloads = own_downloads || segments.iter().flat_map(|s| &s.inner).any(|s| s.downloads);
    let mut targets = HashSet::new();
    let mut writes = HashSet::new();
    for seg in &segments {
        segment_targets(seg, &mut targets);
        segment_writes(seg, &mut writes);
        for inner in &seg.inner {
            targets.extend(inner.targets.iter().cloned());
            writes.extend(inner.writes.iter().cloned());
        }
    }
    download_method_target(&lower, &mut targets);
    StageInfo {
        piped,
        feeds_shell: segments.iter().any(feeds_shell),
        creates_scriptblock: squeezed.contains("[scriptblock]::create"),
        own_downloads,
        downloads,
        targets,
        writes,
        subs,
        segments,
    }
}

/// A redirection word (`>`, `2>&1`, `>>log`, `<`, `*>$null`, `<<<`): its
/// operator and the target glued to it. `<(...)` and `>(...)` are process
/// substitutions, not redirections.
fn redirection(text: &str) -> Option<(&str, &str)> {
    let body = text.trim_start_matches(|c: char| c.is_ascii_digit() || c == '*');
    if body.starts_with("<(") || body.starts_with(">(") {
        return None;
    }
    let op_len = body
        .find(|c: char| !matches!(c, '<' | '>' | '&' | '|'))
        .unwrap_or(body.len());
    let op = &body[..op_len];
    op.contains(['<', '>']).then(|| (op, &body[op_len..]))
}

/// See `Segment::args` and `Segment::stdin_from`.
fn command_args(toks: &[Tok]) -> (Vec<Tok>, Option<String>) {
    let mut args = Vec::new();
    let mut stdin_from = None;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if !t.quoted && t.text.starts_with('#') {
            break;
        }
        let Some((op, glued)) = redirection(&t.text).filter(|_| !t.quoted) else {
            args.push(t.clone());
            i += 1;
            continue;
        };
        i += 1;
        // `2>&1`, `>&2`, `<&-`: a descriptor, no file.
        let dup = op.ends_with('&') && !glued.is_empty();
        let target = if !glued.is_empty() || dup {
            (!dup).then(|| glued.to_string())
        } else if let Some(n) = toks.get(i) {
            i += 1;
            // `< <(curl x)`: the process substitution spans words.
            let mut open = n.text.matches('(').count() as i64 - n.text.matches(')').count() as i64;
            while open > 0 && i < toks.len() {
                open += toks[i].text.matches('(').count() as i64
                    - toks[i].text.matches(')').count() as i64;
                i += 1;
            }
            Some(n.text.clone())
        } else {
            None
        };
        if op == "<" {
            stdin_from = target;
        }
    }
    (args, stdin_from)
}

/// `<(curl ...)`: a process substitution that downloads.
fn is_download_procsub(word: &str) -> bool {
    word.starts_with("<(") && word.split('(').skip(1).any(is_downloader)
}

/// Command word of a token: `iex(iwr` is `iex`.
fn cmd_word(text: &str) -> String {
    let text = text.trim_start_matches(['(', '{']);
    norm_cmd(text.split(['(', '{', ')']).next().unwrap_or(""))
}

fn clean(word: &str) -> String {
    norm_cmd(
        word.trim_matches(|c| matches!(c, '(' | ')' | '$' | '@' | '\'' | '"' | ';' | '&' | '<')),
    )
}

fn is_downloader(word: &str) -> bool {
    DOWNLOADERS.contains(&clean(word).as_str())
}

/// A downloader in command position: first word, after a `=` word, or
/// opening a parenthesis (`iex (irm x)`, `$(curl x)`, `$s=iwr x`). A shell
/// assignment such as `TOOL=curl` is not one.
fn has_download(seg: &Segment) -> bool {
    let first = cmd_word(&seg.toks[0].text);
    if DOWNLOADERS.contains(&first.as_str()) || first == "fetch" || windows_download(seg) {
        return true;
    }
    seg.toks.iter().enumerate().any(|(i, t)| {
        let powershell_var = t.text.starts_with('$');
        (i > 0 && seg.toks[i - 1].text == "=" && is_downloader(&t.text))
            || t.text.split('(').skip(1).any(is_downloader)
            || (powershell_var && t.text.split('=').skip(1).any(is_downloader))
    })
}

/// `certutil -urlcache ... URL file` and `bitsadmin /transfer ... URL file`.
fn windows_download(seg: &Segment) -> bool {
    let verbs: &[&str] = match cmd_word(&seg.toks[0].text).as_str() {
        "certutil" => &["-urlcache", "/urlcache"],
        "bitsadmin" => &["/transfer", "/addfile"],
        _ => return false,
    };
    seg.toks[1..]
        .iter()
        .any(|t| verbs.contains(&t.text.to_ascii_lowercase().as_str()))
}

/// An option of a shell or interpreter whose value is the code it runs.
fn inline_code_option(first: &str, word: &str) -> bool {
    if POSIX_SHELLS.contains(&first) {
        return word.starts_with('-') && !word.starts_with("--") && word.ends_with('c');
    }
    match first {
        "powershell" | "pwsh" => {
            let name = word.to_ascii_lowercase();
            name.len() >= 2 && "-command".starts_with(name.as_str())
        }
        "python" | "python3" | "py" => word == "-c",
        "node" => matches!(word, "-e" | "-p" | "--eval" | "--print"),
        "php" => word == "-r",
        "perl" | "ruby" => inline_option(first, word),
        _ => false,
    }
}

/// See `Segment::code_downloads`. Options after the script word belong to
/// the script (`python3 app.py -c "$(curl x)"` passes data); single-quoted
/// code is not expanded by the calling shell.
fn inline_code_downloads(part: &str, toks: &[Tok], depth: usize) -> bool {
    let first = cmd_word(&toks[0].text);
    let shell = POSIX_SHELLS.contains(&first.as_str()) || WIN_SHELLS.contains(&first.as_str());
    if !shell && !INTERPRETERS.contains(&first.as_str()) {
        return false;
    }
    let raw = |t: &Tok| &part[t.start..t.end];
    let downloads = |code: &str| {
        subexpressions(code)
            .iter()
            .any(|sub| analyze(sub, depth + 1).iter().any(|s| s.downloads))
    };
    let mut i = 1;
    while i < toks.len() {
        let word = toks[i].text.as_str();
        if let Some(glued) = word.strip_prefix("<<<") {
            // The here-string is the script when nothing else is.
            let reads_stdin = if shell {
                script_word(&words(&toks[1..i]), "-file").is_none()
            } else {
                interpreter_script(&first, &toks[1..i]) == Some(None)
            };
            let code = if glued.is_empty() {
                toks.get(i + 1).map(raw)
            } else {
                Some(raw(&toks[i]))
            };
            return reads_stdin && code.is_some_and(downloads);
        }
        if inline_code_option(&first, word) {
            return toks.get(i + 1).map(raw).is_some_and(downloads);
        }
        if word == "-" {
            // Standard input as the script: `python3 - <<< "..."`.
            i += 1;
            continue;
        }
        if word.starts_with('-') && word.len() >= 2 {
            if VALUE_FLAGS.contains(&word.to_ascii_lowercase().as_str())
                || interpreter_value_option(&first, word)
            {
                i += 1;
            }
        } else if !word.is_empty() && !is_cmd_switch(word) {
            return false;
        }
        i += 1;
    }
    false
}

/// `<(curl ...)` handed to a shell or `source`.
fn feeds_shell(seg: &Segment) -> bool {
    seg.toks
        .iter()
        .any(|t| t.text.starts_with("<(") && t.text.split('(').skip(1).any(is_downloader))
}

// ---------------------------------------------------------------------------
// What a download is written to
// ---------------------------------------------------------------------------

/// `${name}` written as `$name`, so both spellings of a variable match.
fn unbrace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("${") {
        match rest[i + 2..].find('}') {
            Some(j) => {
                out.push_str(&rest[..i]);
                out.push('$');
                out.push_str(&rest[i + 2..i + 2 + j]);
                rest = &rest[i + 3 + j..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Last path part of a file name or URL (`d\a.ps1`, `https://x/a.zip?v=1`),
/// lower-cased; empty when there is none (`http://x/dist/`).
fn base_name(word: &str) -> String {
    let word = unbrace(word.trim_matches(|c| matches!(c, '\'' | '"' | '(' | ')' | ';')));
    let word = word.split(['?', '#']).next().unwrap_or("");
    word.rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn add_name(set: &mut HashSet<String>, word: &str) {
    let name = base_name(word);
    if !name.is_empty() && !name.starts_with('-') && name != "." && name != ".." {
        set.insert(name);
    }
}

fn is_url(word: &str) -> bool {
    word.contains("://")
}

/// Output files, the name taken from the URL for `-O`, and variables that
/// receive the output, of a downloader command.
fn segment_targets(seg: &Segment, out: &mut HashSet<String>) {
    let first = cmd_word(&seg.toks[0].text);
    // `$s = iwr x`, `$s=iwr x`, `X=$(curl x)`: the variable holds the download.
    if let Some((i, _)) = seg.toks.iter().enumerate().find(|(i, t)| {
        (*i > 0 && seg.toks[*i - 1].text == "=" && is_downloader(&t.text))
            || t.text.split(['(', '=']).skip(1).any(is_downloader)
    }) {
        let var = if i > 1 && seg.toks[i - 1].text == "=" {
            Some(seg.toks[i - 2].text.as_str())
        } else {
            seg.toks[i].text.split('=').next().filter(|v| !v.is_empty())
        };
        if let Some(v) = var.filter(|v| {
            v.trim_start_matches('$')
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_')
        }) {
            let name = v.trim_start_matches('$').to_ascii_lowercase();
            // A POSIX variable (`x=$(curl ..)`) runs as a bare `$x` command
            // word; a PowerShell variable (`$x = irm ..`) as a statement
            // only prints.
            if !v.starts_with('$') {
                out.insert(format!("!posix${name}"));
            }
            out.insert(format!("${name}"));
        }
    }
    if first == "start-bitstransfer" {
        // `Start-BitsTransfer <source> <destination>`; without a destination
        // the file lands in the current folder under the URL's name.
        for t in &seg.toks[1..] {
            if !t.text.starts_with('-') {
                add_name(out, &t.text);
            }
        }
    }
    if windows_download(seg) {
        // The local file follows its URL: `-f URL file`, `/transfer job URL
        // file` (also several pairs). Without one, the name in the URL.
        let positional: Vec<&str> = seg.toks[1..]
            .iter()
            .map(|t| t.text.as_str())
            .filter(|w| !w.starts_with('-') && !is_cmd_switch(w))
            .collect();
        let files: Vec<&&str> = positional
            .iter()
            .skip_while(|w| !is_url(w))
            .filter(|w| !is_url(w))
            .collect();
        if files.is_empty() {
            positional
                .iter()
                .filter(|w| is_url(w))
                .for_each(|w| add_name(out, w));
        }
        files.into_iter().for_each(|w| add_name(out, w));
        return;
    }
    if !DOWNLOADERS.contains(&first.as_str()) {
        return;
    }
    let args = &seg.toks[1..];
    let mut remote_name = false;
    let mut stdout_or_file = false;
    for (i, t) in args.iter().enumerate() {
        let text = t.text.as_str();
        let next = args.get(i + 1).map(|n| n.text.as_str());
        let name = text.to_ascii_lowercase();
        let name = name.split([':', '=']).next().unwrap_or("");
        let cluster = text.starts_with('-') && !text.starts_with("--") && text.len() > 1;
        if matches!(
            name,
            "-outfile" | "-destination" | "--output" | "--output-document" | "-outputdocument"
        ) {
            // `-OutFile:x`, `--output=x` carry the value; else the next word.
            match text.split_once([':', '=']) {
                Some((_, v)) if !v.is_empty() => add_name(out, v),
                _ => {
                    if let Some(n) = next {
                        add_name(out, n);
                    }
                }
            }
            stdout_or_file = true;
        } else if first == "curl" && cluster {
            // `-fsSLo file`, `-Lofile`, `-O`: `o` takes the file, `O` the URL name.
            for (idx, ch) in text.char_indices().skip(1).take(9) {
                if ch == 'O' {
                    remote_name = true;
                } else if !ch.is_ascii_alphabetic() {
                    break;
                } else if ch == 'o' {
                    match &text[idx + 1..] {
                        "" => {
                            if let Some(n) = next {
                                add_name(out, n);
                            }
                        }
                        glued => add_name(out, glued),
                    }
                    break;
                }
            }
        } else if first == "wget" && cluster {
            // `-O file`, `-qO file`, `-Ofile`; `-O-` and `-qO-` write to stdout.
            if let Some(idx) = text.find('O') {
                match (&text[idx + 1..], next) {
                    ("-", _) => {}
                    ("", Some(n)) if n != "-" => add_name(out, n),
                    ("", _) => {}
                    (file, _) => add_name(out, file),
                }
                stdout_or_file = true;
            }
        } else if name == "-o" && first != "curl" && first != "wget" {
            if let Some(n) = next {
                add_name(out, n);
            }
            stdout_or_file = true;
        } else if first == "curl" && text == "--remote-name" {
            remote_name = true;
        }
    }
    // curl -O and plain wget save under the name in the URL.
    if remote_name || (first == "wget" && !stdout_or_file) {
        for t in args.iter().filter(|t| is_url(&t.text)) {
            add_name(out, &t.text);
        }
    }
}

/// Files written by redirections (`> a`, `>>a`, `2>a`) and by writer commands.
fn segment_writes(seg: &Segment, out: &mut HashSet<String>) {
    for (i, t) in seg.toks.iter().enumerate() {
        let text = t.text.trim_start_matches(|c: char| c.is_ascii_digit());
        if let Some(rest) = text.strip_prefix('>') {
            let rest = rest.trim_start_matches(['>', '|']);
            if rest.is_empty() {
                if let Some(n) = seg.toks.get(i + 1) {
                    add_name(out, &n.text);
                }
            } else if !rest.starts_with('&') {
                add_name(out, rest);
            }
        }
    }
    if WRITERS.contains(&cmd_word(&seg.toks[0].text).as_str()) {
        for t in &seg.toks[1..] {
            if !t.text.starts_with('-') {
                add_name(out, &t.text);
            }
        }
    }
}

/// `(New-Object Net.WebClient).DownloadFile('https://x','a.exe')`: the last
/// argument is the file.
fn download_method_target(lower: &str, out: &mut HashSet<String>) {
    for method in [".downloadfile(", ".downloaddata("] {
        if let Some(i) = lower.find(method) {
            let args = &lower[i + method.len()..];
            let args = args.split(')').next().unwrap_or("");
            if let Some(last) = args.rsplit(',').next() {
                add_name(out, last.trim());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Words and path parts of a text: `d\a.ps1` gives `d`, `a.ps1`. Flags are
/// left out.
fn pieces(lower: &str) -> HashSet<String> {
    unbrace(lower)
        .split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ',' | '('
                        | ')'
                        | '\''
                        | '"'
                        | '='
                        | '/'
                        | '\\'
                        | ';'
                        | '|'
                        | '&'
                        | '<'
                        | '>'
                        | '{'
                        | '}'
                )
        })
        .filter(|p| !p.is_empty() && !p.starts_with('-') && !matches!(*p, "." | ".."))
        .map(str::to_string)
        .collect()
}

/// Does any word of the arguments name a downloaded file? For commands that
/// run whatever they are given (`eval`, `msiexec /i a.msi`).
fn names_downloaded(args: &[Tok], downloaded: &HashSet<String>) -> bool {
    args.iter().any(|t| {
        pieces(&t.text.to_ascii_lowercase())
            .iter()
            .any(|p| downloaded.contains(p))
    })
}

/// Is this word (a command, a script path, `$var`) a downloaded file or the
/// variable that holds a download? Only the last path part counts, so
/// `./out/run.sh` is not `out`.
fn is_downloaded(word: &str, downloaded: &HashSet<String>) -> bool {
    downloaded.contains(&base_name(word))
}

/// `a.exe`, `.\tool`, `bin/tool`: a program given by file name or path.
fn is_file_path(lower: &str) -> bool {
    lower.contains(['/', '\\']) || EXEC_EXTS.iter().any(|e| lower.ends_with(e))
}

/// The script or program a command runs: the value of `file_flag` (`-File`,
/// `-FilePath`) or else the first word that is not an option or its value.
/// Later words are data.
fn script_word(args: &[String], file_flag: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let word = args[i].as_str();
        let lower = word.to_ascii_lowercase();
        let (name, value) = match lower.split_once(':') {
            Some((n, _)) if n.starts_with('-') => (n, Some(&word[n.len() + 1..])),
            _ => (lower.as_str(), None),
        };
        if word.is_empty() || word == "-" {
            // `start "" a.exe`: the empty window title; `-`: standard input.
        } else if name.starts_with('-') && name.len() >= 2 {
            if !file_flag.is_empty() && file_flag.starts_with(name) {
                return match value {
                    Some(v) if !v.is_empty() => Some(v.to_string()),
                    _ => args.get(i + 1).cloned(),
                };
            }
            if VALUE_FLAGS.contains(&name) && value.is_none() {
                i += 1;
            }
        } else if is_cmd_switch(word) {
            // cmd switch: `/c`, `/i`, `/v:on`.
        } else {
            return Some(word.to_string());
        }
        i += 1;
    }
    None
}

fn words(args: &[Tok]) -> Vec<String> {
    args.iter().map(|t| t.text.clone()).collect()
}

/// An option of this interpreter that runs inline code or only checks syntax,
/// so no script file runs: `python -c`, `node --check`, `php -l`, `perl -ne`.
fn inline_option(interp: &str, word: &str) -> bool {
    // A perl or ruby cluster ending in `e` (`-ne`, `-lane`); not a glued
    // value such as `-Mstrict` or `-rjson`.
    let e_cluster = |code: &[char], valued: &str| {
        let rest = word.strip_prefix('-').unwrap_or("");
        !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_alphabetic())
            && rest.ends_with(code)
            && !rest.starts_with(|c: char| valued.contains(c))
    };
    match interp {
        "node" => matches!(word, "-e" | "-p" | "-c" | "--check"),
        "python" | "python3" | "py" => {
            // `-c code`, `-m module`, also glued: `-mjson.tool`.
            !word.starts_with("--") && (word.starts_with("-c") || word.starts_with("-m"))
        }
        "php" => word == "-l" || (word.starts_with("-r") && !word.starts_with("--")),
        "perl" => e_cluster(&['e', 'E'], "MmIxCdD"),
        "ruby" => e_cluster(&['e'], "rIECKFx"),
        _ => false,
    }
}

/// Options of each interpreter that take the next word as their value.
fn interpreter_value_option(interp: &str, word: &str) -> bool {
    match interp {
        "python" | "python3" | "py" => matches!(word, "-W" | "-X"),
        "node" => matches!(
            word,
            "-r" | "--require"
                | "--import"
                | "--loader"
                | "--experimental-loader"
                | "-C"
                | "--conditions"
                | "--env-file"
        ),
        "ruby" => matches!(word, "-I" | "-r" | "-C" | "-E"),
        "php" => matches!(word, "-c" | "-d" | "-z"),
        _ => false,
    }
}

/// What an interpreter runs: `None` for inline code (an inline-code or
/// syntax-check option before the script), else `Some(script word)`, where no
/// script word or `-` means standard input. Options after the script word are
/// the script's own arguments (`convert.py -o -` writes to standard output).
fn interpreter_script(interp: &str, args: &[Tok]) -> Option<Option<String>> {
    let mut i = 0;
    while i < args.len() {
        let word = args[i].text.as_str();
        if word == "-" {
            break;
        }
        if word.starts_with('-') && word.len() >= 2 {
            if inline_option(interp, word) {
                return None;
            }
            if interpreter_value_option(interp, word) {
                i += 1;
            }
        } else if !word.is_empty() && !is_cmd_switch(word) {
            return Some(Some(word.to_string()));
        }
        i += 1;
    }
    Some(None)
}

/// Data files: opening one by association shows it, it does not run code.
const DATA_EXTS: &[&str] = &[
    ".txt", ".log", ".md", ".json", ".yaml", ".yml", ".xml", ".csv", ".tsv", ".html", ".htm",
    ".pdf", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".mp3", ".mp4", ".wav", ".zip",
    ".tar", ".gz", ".7z", ".docx", ".xlsx", ".pptx", ".ini", ".conf", ".toml",
];

/// A launched file counts when it is downloaded, and, for launchers that open
/// by file association (`start`, `Start-Process`, `ii`), when it is not a data
/// file.
fn launched(first: &str, word: &str, downloaded: &HashSet<String>) -> bool {
    is_downloaded(word, downloaded)
        && (!matches!(
            first,
            "start" | "start-process" | "saps" | "ii" | "invoke-item"
        ) || !DATA_EXTS.iter().any(|e| base_name(word).ends_with(e)))
}

/// cmd `start` switches; `/d`, `/node` and `/affinity` take a value.
const START_SWITCHES: &[&str] = &[
    "wait",
    "min",
    "max",
    "b",
    "i",
    "separate",
    "shared",
    "low",
    "normal",
    "high",
    "realtime",
    "abovenormal",
    "belownormal",
];
const START_VALUE_SWITCHES: &[&str] = &["d", "node", "affinity"];

/// The words `start` may run: the first positional word, and after a quoted
/// title (`start "" a.exe`, `start "My App" a.exe`) the next one. Switches
/// (`/wait`, `/d <dir>`, `/Dc:\dir`) and PowerShell parameters are skipped.
fn start_programs(args: &[Tok]) -> Vec<&Tok> {
    let mut out: Vec<&Tok> = Vec::new();
    let mut i = 0;
    while i < args.len() && out.len() < 2 {
        let t = &args[i];
        let lower = t.text.to_ascii_lowercase();
        let switch = lower.strip_prefix('/').filter(|_| !t.quoted);
        if let Some(name) = switch.filter(|n| {
            START_SWITCHES.contains(n)
                || START_VALUE_SWITCHES.contains(n)
                || (n.starts_with('d') && !n.contains('/'))
                || is_cmd_switch(&t.text)
        }) {
            // `/d <dir>` takes the next word; `/Dc:\dir` carries it.
            if START_VALUE_SWITCHES.contains(&name) {
                i += 1;
            }
        } else if lower.starts_with('-') && lower.len() >= 2 {
            if VALUE_FLAGS.contains(&lower.as_str()) {
                i += 1;
            }
        } else if t.text.is_empty() {
            // `start "" a.exe`: an empty title.
        } else if out.first().is_none_or(|f| f.quoted) {
            out.push(t);
        } else {
            break;
        }
        i += 1;
    }
    out
}

/// Is this word a cmd switch (`/c`, `/v:on`)?
fn is_cmd_switch(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('/') else {
        return false;
    };
    let (name, value) = match rest.split_once(':') {
        Some((n, v)) => (n, Some(v)),
        None => (rest, None),
    };
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphabetic())
        && (name.len() <= 2 || value.is_some())
        && match value {
            Some(v) => !v.contains(['/', '\\']),
            None => true,
        }
}

/// Does the program that `Start-Process`/`start`/`ii` launches, or the shell,
/// interpreter or file runner it starts, run a downloaded file?
fn launcher_runs(first: &str, args: &[Tok], downloaded: &HashSet<String>) -> bool {
    let all = words(args);
    let file_flag = match first {
        "invoke-command" | "icm" | "start-process" | "saps" => "-filepath",
        _ => "",
    };
    // Words before `-ArgumentList` belong to the launcher.
    let cut = all
        .iter()
        .position(|w| {
            let l = w.to_ascii_lowercase();
            l == "-args" || (l.len() >= 2 && "-argumentlist".starts_with(&l))
        })
        .unwrap_or(all.len());
    let program = if first == "start" {
        // `start "title" a.exe`: after a quoted title the program is next.
        let programs = start_programs(&args[..cut]);
        if programs
            .iter()
            .any(|t| launched(first, &t.text, downloaded))
        {
            return true;
        }
        programs.last().map(|t| t.text.clone())
    } else {
        script_word(&all[..cut], file_flag)
    };
    let Some(program) = program else {
        return false;
    };
    if launched(first, &program, downloaded) {
        return true;
    }
    // The words after the program: its own arguments.
    let at = all.iter().position(|w| *w == program).map_or(0, |i| i + 1);
    let rest: Vec<String> = all[at..]
        .iter()
        .flat_map(|w| w.split(|c: char| c.is_whitespace() || c == ','))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let pname = norm_cmd(&program);
    if FILE_RUNNERS.contains(&pname.as_str()) {
        // `Start-Process msiexec -ArgumentList '/i setup.msi'`.
        return rest.iter().any(|w| {
            pieces(&w.to_ascii_lowercase())
                .iter()
                .any(|p| downloaded.contains(p))
        });
    }
    // `Start-Process powershell -ArgumentList '-File','a.ps1'`.
    if POSIX_SHELLS.contains(&pname.as_str())
        || WIN_SHELLS.contains(&pname.as_str())
        || INTERPRETERS.contains(&pname.as_str())
    {
        return script_word(&rest, "-file").is_some_and(|w| is_downloaded(&w, downloaded));
    }
    false
}

/// The script word of a shell or interpreter and whether it reads its script
/// from standard input; `None` for inline code or another command.
fn script_of(first: &str, seg: &Segment) -> Option<(Option<String>, bool)> {
    let args = &seg.args;
    if POSIX_SHELLS.contains(&first) || WIN_SHELLS.contains(&first) {
        let script = script_word(&words(args), "-file");
        let stdin = dash_script(args)
            || stdin_script(script.as_deref())
            || args.iter().any(|t| t.text == "-s");
        return Some((script, stdin));
    }
    if INTERPRETERS.contains(&first) {
        let script = interpreter_script(first, args)?;
        let stdin = stdin_script(script.as_deref());
        return Some((script, stdin));
    }
    None
}

/// A shell given `-` where its script goes (`pwsh -`, `bash -x -`); a `-`
/// after the script word is the script's own argument.
fn dash_script(args: &[Tok]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let word = args[i].text.as_str();
        if word == "-" {
            return true;
        }
        if word.starts_with('-') && word.len() >= 2 {
            if VALUE_FLAGS.contains(&word.to_ascii_lowercase().as_str()) {
                i += 1;
            }
        } else if !word.is_empty() && !is_cmd_switch(word) {
            return false;
        }
        i += 1;
    }
    false
}

/// No script word, or one that names standard input (`pwsh -File -`).
fn stdin_script(script: Option<&str>) -> bool {
    match script {
        None => true,
        Some(w) => matches!(w, "-" | "/dev/stdin" | "/dev/fd/0" | "/proc/self/fd/0"),
    }
}

/// A call operator or dot-source target that is computed: `& (...)`, `& $f`,
/// `. <(curl x)`.
fn is_dynamic_target(text: &str) -> bool {
    text.starts_with(['(', '$', '<', '['])
}

fn is_call(seg: &Segment) -> bool {
    seg.raw_first == "&" || seg.raw_first == "." || seg.raw_first.starts_with('&')
}

/// Does the stage run, by its own words, code that it downloads itself?
fn runs_itself(info: &StageInfo) -> Option<bool> {
    if let Some(seg) = info.segments.iter().find(|s| s.code_downloads) {
        return Some(POSIX_SHELLS.contains(&cmd_word(&seg.toks[0].text).as_str()));
    }
    if !info.own_downloads {
        return None;
    }
    if info.creates_scriptblock {
        return Some(false);
    }
    for seg in &info.segments {
        let first = cmd_word(&seg.toks[0].text);
        if is_call(seg) && is_dynamic_target(&seg.toks[0].text) {
            return Some(false);
        }
        if matches!(first.as_str(), "iex" | "invoke-expression" | "eval")
            || LAUNCHERS.contains(&first.as_str())
        {
            return Some(false);
        }
        let is_shell =
            POSIX_SHELLS.contains(&first.as_str()) || WIN_SHELLS.contains(&first.as_str());
        // The command a shell runs, or its script, is the substitution itself;
        // `bash deploy.sh $(curl ..)` only passes it as an argument.
        let substitution = script_word(&words(&seg.args), "-file")
            .is_some_and(|w| w.starts_with("$(") || w.starts_with('`'));
        // `bash <(curl x)`, `source <(curl x)`, ``bash -c "`curl x`"``.
        if (info.feeds_shell && (first == "source" || is_shell)) || (is_shell && substitution) {
            return Some(POSIX_SHELLS.contains(&first.as_str()));
        }
        // `python3 <(curl x)`, `python3 - < <(curl x)`, `bash < <(curl x)`.
        if let Some((script, stdin)) = script_of(&first, seg) {
            if script.as_deref().is_some_and(is_download_procsub)
                || (stdin && seg.stdin_from.as_deref().is_some_and(is_download_procsub))
            {
                return Some(POSIX_SHELLS.contains(&first.as_str()));
            }
        }
    }
    None
}

/// Does a stage after a download run it (or code that it produced)?
/// `Some(true)` when through a POSIX shell.
fn runs_later(
    info: &StageInfo,
    piped: bool,
    downloaded: &HashSet<String>,
    depth: usize,
) -> Option<bool> {
    if info.creates_scriptblock {
        return Some(false);
    }
    for seg in &info.segments {
        let first = cmd_word(&seg.toks[0].text);
        let first_raw = seg.toks[0].text.to_ascii_lowercase();
        let args = &seg.args[..];
        // `sh < a.sh`: standard input is a downloaded file.
        let fed = piped
            || seg
                .stdin_from
                .as_deref()
                .is_some_and(|w| is_downloaded(w, downloaded));
        if is_call(seg) {
            let target = &seg.toks[0].text;
            if is_dynamic_target(target) {
                return Some(false);
            }
            // `& curl ...` is only a call; `& .\a.ps1` runs a downloaded file.
            if !is_downloader(target) && is_downloaded(target, downloaded) {
                return Some(false);
            }
            continue;
        }
        if matches!(first.as_str(), "iex" | "invoke-expression") {
            return Some(false);
        }
        // `x=$(curl ..); $x`: a POSIX variable as the command runs. A
        // PowerShell `$r` as a statement only prints, except as the text a
        // wrapper runs (`powershell -c "$c; exit"`, `bash -c "$x"`).
        if first_raw.starts_with('$')
            && (downloaded.contains(&format!("!posix{}", base_name(&first_raw)))
                || (depth > 0 && is_downloaded(&first_raw, downloaded)))
        {
            return Some(false);
        }
        let is_posix = POSIX_SHELLS.contains(&first.as_str());
        if is_posix || WIN_SHELLS.contains(&first.as_str()) {
            if fed && dash_script(args) {
                // `curl x | pwsh -`: the script comes from standard input.
                return Some(is_posix);
            }
            if !seg.inner.is_empty() {
                // `powershell -c "..."`: what runs is the inner text; it
                // runs the download when it consumes it, also when its
                // command is the variable that holds it (`powershell -c $c`).
                if detect_inner(&seg.inner, piped, downloaded, depth + 1).is_some() {
                    return Some(is_posix);
                }
            } else {
                let script = script_word(&words(args), "-file");
                let reads_stdin =
                    stdin_script(script.as_deref()) || args.iter().any(|t| t.text == "-s");
                if (fed && reads_stdin) || script.is_some_and(|w| is_downloaded(&w, downloaded)) {
                    return Some(is_posix);
                }
            }
        } else if INTERPRETERS.contains(&first.as_str()) {
            // `python3 - --version`: `-` is standard input whatever follows.
            if let Some(script) = interpreter_script(&first, args) {
                if (fed && stdin_script(script.as_deref()))
                    || script.is_some_and(|w| is_downloaded(&w, downloaded))
                {
                    return Some(false);
                }
            }
        } else if first == "source" {
            if script_word(&words(args), "").is_some_and(|w| is_downloaded(&w, downloaded)) {
                return Some(false);
            }
        } else if first == "eval" || FILE_RUNNERS.contains(&first.as_str()) {
            if names_downloaded(args, downloaded) {
                return Some(false);
            }
        } else if LAUNCHERS.contains(&first.as_str()) {
            if launcher_runs(&first, args, downloaded) {
                return Some(false);
            }
        } else if is_file_path(&first_raw) && is_downloaded(&first_raw, downloaded) {
            return Some(false);
        }
    }
    None
}

/// Does the text of a wrapper, run after a download, run it? The first inner
/// stage reads what the wrapper's stage reads.
fn detect_inner(
    stages: &[StageInfo],
    piped: bool,
    downloaded: &HashSet<String>,
    depth: usize,
) -> Option<bool> {
    if depth > MAX_UNWRAP_DEPTH {
        return None;
    }
    stages.iter().enumerate().find_map(|(i, info)| {
        let piped = info.piped || (i == 0 && piped);
        runs_later(info, piped, downloaded, depth)
    })
}

/// Find a download followed by execution in the stages of one text.
fn detect(
    stages: &[StageInfo],
    piped: bool,
    downloaded: &HashSet<String>,
    depth: usize,
) -> Option<bool> {
    let mut downloaded = downloaded.clone();
    let mut seen_download = false;
    for (i, info) in stages.iter().enumerate() {
        let piped = info.piped || (i == 0 && piped);
        if seen_download {
            if let Some(posix) = runs_later(info, piped, &downloaded, depth) {
                return Some(posix);
            }
        }
        if let Some(posix) = runs_itself(info) {
            return Some(posix);
        }
        if info.downloads {
            seen_download = true;
            downloaded.extend(info.targets.iter().cloned());
        }
        if seen_download {
            downloaded.extend(info.writes.iter().cloned());
        }
    }
    None
}
