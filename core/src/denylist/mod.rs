//! Local command classifier and denylist.
//!
//! Applied after a command parses (M6) but before the user-confirmation UI
//! (M8b). The classifier is INDEPENDENT of `risk` declared by ChatGPT — per
//! `docs/protocol.md` §2.4 trust rule and `docs/security.md` §3.
//!
//! Two outcomes per command:
//!   - `Block`: hard-blocked. The confirmation UI will NOT offer `[r]un`;
//!     user can only `[e]dit`, `[s]kip`, or `[q]uit`.
//!   - `Warn`: shown to the user as warnings; `[r]un` is still offered.
//!
//! Matching strategy: we tokenise the command string on pipe / `&&` / `;` /
//! newline / subshell boundaries so a hostile stage anywhere in a pipeline
//! blocks the whole command. We then apply each rule to each stage
//! individually.
//!
//! PowerShell and cmd rules apply on every OS: on Windows ChatGPT's commands
//! run in PowerShell, and it may send PowerShell on macOS/Linux too. Matching
//! is case-insensitive and understands the common PowerShell aliases
//! (`ls`/`gci`, `cat`/`gc`, `rm`/`ri`/`rd`, `iex`, `iwr`, ...). Commands
//! wrapped in `powershell -Command "..."`, `pwsh -c '...'` or `cmd /c "..."`
//! are classified by their inner text.

mod autorun;
mod download;
mod readonly;
mod secrets;
mod shell;

pub use autorun::auto_run_programs;
use readonly::{infer_stage_risk, max_risk};
use regex::Regex;
pub use secrets::{
    names_secret_location, path_is_secret, SecretLocation, DOTENV_TEMPLATES, SECRET_LOCATIONS,
};
use shell::{
    apply_windows_block_rules, canonical_stage, command_tokens, escapes_an_operator, norm_cmd,
    normalize, posix_reading, skip_exec_prefixes, subexpressions,
};
use std::borrow::Cow;
use std::cell::OnceCell;
use std::sync::OnceLock;

/// How deep wrappers (`powershell -c "..."`, `cmd /c`, `bash -c`, `iex "..."`)
/// and `$(...)` are unwrapped. Text inside the last level is not unwrapped
/// further: it is not examined for what it runs, and its risk is `Unknown`,
/// never `ReadOnly`.
pub(super) const MAX_UNWRAP_DEPTH: usize = 8;

// Test-only counter of `apply_block_rules` invocations, so
// `nested_wrappers_are_bounded` asserts a deterministic bound instead of a
// wall-clock time that flakes under parallel load.
#[cfg(test)]
thread_local! {
    pub(super) static RULE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub local_risk: LocalRisk,
    pub blocks: Vec<RuleHit>,
    pub warnings: Vec<RuleHit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRisk {
    /// No write side effects detected; no blocks; no warnings.
    ReadOnly,
    /// Modifies local files but nothing severe.
    WriteLocal,
    /// Talks to the network.
    Network,
    /// Destructive or otherwise high-impact, but not denylisted.
    Destructive,
    /// At least one hard-block rule matched.
    Blocked,
    /// Could not classify; treat as suspicious.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleHit {
    pub rule_name: &'static str,
    pub stage: String,
    pub note: String,
}

pub fn classify(command: &str) -> Classification {
    // PowerShell backtick escapes (`R`emove-It`em`) are removed and dashes
    // spelled `–`/`—` become `-` before any matching; a backtick anywhere also
    // forbids `ReadOnly` (POSIX command substitution), as does `$'...'`
    // (POSIX ANSI-C quoting, which the splitter does not read), as does a
    // backslash escaping a quote or an operator (the splitter reads no
    // escapes, Ruling 49).
    let unclear_quoting =
        command.contains('`') || command.contains("$'") || escapes_an_operator(command);
    // How a POSIX shell reads the same text, for the download rule.
    let posix = posix_reading(command);
    let normalized = normalize(command);
    let command = normalized.as_str();
    let stages = split_stages(command);
    let whole = Whole::new(command);
    let mut blocks = Vec::new();
    let mut warnings = Vec::new();
    let mut risk = LocalRisk::ReadOnly;

    for stage in &stages {
        let s = stage.trim();
        if s.is_empty() {
            continue;
        }
        apply_block_rules(s, &mut blocks, 0, &whole);
        apply_warn_rules(s, &mut warnings);
        risk = max_risk(risk, infer_stage_risk(s, 0));
        // A secrets read is never `ReadOnly`, whatever reads it (B6).
        if names_secret_location(s) {
            warnings.push(RuleHit {
                rule_name: "secret_location",
                stage: s.to_string(),
                note: "names a known secrets location (.env, .ssh, keys, credentials, \
                       browser profile, keychain): treated as a secrets read."
                    .to_string(),
            });
            risk = max_risk(risk, LocalRisk::Unknown);
        }
    }

    // Some patterns span multiple stages (e.g. `curl … | sh`). Run all block
    // rules once more against the unsplit command so cross-stage hostile
    // pipelines are caught. Dedup against per-stage matches by rule_name +
    // stage text equality.
    // A single stage that is the whole command was just checked.
    if stages.len() != 1 || stages[0].trim() != command.trim() {
        let mut whole_blocks: Vec<RuleHit> = Vec::new();
        apply_block_rules(command, &mut whole_blocks, 0, &whole);
        for hit in whole_blocks {
            push_hit(&mut blocks, hit);
        }
    }

    if let Some(posix) = &posix {
        download::apply_as(posix, command, &mut blocks);
    }

    if unclear_quoting && risk == LocalRisk::ReadOnly {
        risk = LocalRisk::Unknown;
    }
    if !blocks.is_empty() {
        risk = LocalRisk::Blocked;
    }

    Classification {
        local_risk: risk,
        blocks,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Block rules — denylist per security.md §6
// ---------------------------------------------------------------------------

fn block_rules() -> &'static [(&'static str, Regex, &'static str)] {
    static R: OnceLock<Vec<(&'static str, Regex, &'static str)>> = OnceLock::new();
    R.get_or_init(|| {
        let mk = |name: &'static str, pat: &str, note: &'static str| {
            (name, Regex::new(pat).expect("denylist regex compiles"), note)
        };
        vec![
            // Privilege escalation
            mk(
                "sudo",
                r"(?:^|\s)sudo(?:\s|$)",
                "sudo is denylisted in v0.1 (security.md §6).",
            ),
            mk(
                "su",
                r"(?:^|\s)su(?:\s|$)",
                "su is denylisted in v0.1.",
            ),
            mk(
                "pkexec",
                r"(?:^|\s)pkexec(?:\s|$)",
                "pkexec is denylisted in v0.1.",
            ),
            mk(
                "doas",
                r"(?:^|\s)doas(?:\s|$)",
                "doas is denylisted in v0.1.",
            ),
            // Recursive root/home destruction
            // The target is the whole word: `/`, `/usr`, `~`, `$HOME`,
            // `/Users/<name>`, `/home/<name>`, each also with `/` or `/*`;
            // `/tmp/build` and `~/proj/build` are not it.
            mk(
                "rm_rf_root_or_home",
                &format!(
                    r#"(?:^|[\s"'])rm\s+(?:-\S*\s+)*(?:-[a-zA-Z]*[rR][a-zA-Z]*|--r(?:e(?:c(?:u(?:r(?:s(?:i(?:ve?)?)?)?)?)?)?)?(?:=\S*)?)\s+(?:-\S*\s+)*["']?(?:/\*?|/{SEG}{ANY_TAIL}|/(?:Users|home)/{SEG}{ANY_TAIL}|(?:~|\$HOME|\$\{{HOME\}}){ANY_TAIL})(?:$|[\s;&|)"'])"#
                ),
                "recursive deletion of root or home directory.",
            ),
            // Raw disk writes
            mk("dd_to_dev", r"(?:^|\s)dd\b.*\bof=/dev/", "dd to /dev/* is denylisted."),
            mk("mkfs", r"(?:^|\s)mkfs(?:\.[a-z0-9]+)?\b", "filesystem creation is denylisted."),
            mk("fdisk", r"(?:^|\s)fdisk\b", "fdisk is denylisted."),
            mk("parted", r"(?:^|\s)parted\b", "parted is denylisted."),
            mk("blkdiscard", r"(?:^|\s)blkdiscard\b", "blkdiscard is denylisted."),
            mk("wipefs", r"(?:^|\s)wipefs\b", "wipefs is denylisted."),
            mk(
                "macos_disk_erase",
                r#"(?i)(?:^|[\s;&|("'/])(?:diskutil\b[^|;&\n]*\s(?:erasedisk|erasevolume|zerodisk|randomdisk|secureerase|partitiondisk|reformat|deletecontainer|deletevolume)\b|newfs_[a-z0-9]+\b)"#,
                "erasing or repartitioning a disk is denylisted.",
            ),
            // Permission/ownership sweeps over root
            mk(
                "chmod_root_recursive",
                r"(?:^|\s)chmod\s+-R\s+/(?:\s|$|\*)",
                "recursive chmod over /.",
            ),
            mk(
                "chown_root_recursive",
                r"(?:^|\s)chown\s+-R\s+/(?:\s|$|\*)",
                "recursive chown over /.",
            ),
            // `curl ... | sh` and other download-then-run forms: see `download.rs`.
            mk(
                "eval_curl_substitution",
                r#"(?:^|\s)(?:bash|sh|zsh|eval)\s+-c\s+["']?\$\(\s*(?:curl|wget|fetch)"#,
                "eval/sh -c \"$(curl ...)\" is denylisted.",
            ),
            // Credential extraction — file paths
            mk(
                "ssh_keys",
                r#"(?i)(?:^|[\s"'<])(?:~|\$HOME|/Users/[^/\s"']+|/home/[^/\s"']+)/\.ssh/"#,
                "access to ~/.ssh files.",
            ),
            mk(
                "aws_credentials",
                r#"(?i)(?:^|[\s"'<])(?:~|\$HOME|/Users/[^/\s"']+|/home/[^/\s"']+)/\.aws/(?:credentials|config)\b"#,
                "access to ~/.aws/credentials.",
            ),
            mk(
                "gcloud_creds",
                r#"(?i)(?:^|[\s"'<])(?:~|\$HOME|/Users/[^/\s"']+|/home/[^/\s"']+)/\.config/gcloud/"#,
                "access to ~/.config/gcloud.",
            ),
            mk(
                "kube_config",
                r#"(?i)(?:^|[\s"'<])(?:~|\$HOME|/Users/[^/\s"']+|/home/[^/\s"']+)/\.kube/config\b"#,
                "access to ~/.kube/config.",
            ),
            mk(
                "netrc",
                r#"(?i)(?:^|[\s"'<])(?:~|\$HOME|/Users/[^/\s"']+|/home/[^/\s"']+)/\.netrc\b"#,
                "access to ~/.netrc.",
            ),
            mk(
                "pgpass",
                r#"(?i)(?:^|[\s"'<])(?:~|\$HOME|/Users/[^/\s"']+|/home/[^/\s"']+)/\.pgpass\b"#,
                "access to ~/.pgpass.",
            ),
            // Credential extraction — commands
            mk(
                "macos_security_keychain",
                r"(?:^|\s)security\s+(?:find-(?:generic|internet)-password|dump-keychain)\b",
                "macOS Keychain extraction.",
            ),
            mk(
                "gh_auth_token",
                r"(?:^|\s)gh\s+auth\s+token\b",
                "gh auth token print.",
            ),
            mk(
                "op_cli",
                r"(?:^|\s)op\s+(?:read|item\s+get|account\s+token)\b",
                "1Password CLI secret read.",
            ),
            mk("bw_cli", r"(?:^|\s)bw\s+(?:get|export)\b", "Bitwarden CLI export."),
            mk(
                "aws_session_token",
                r"(?:^|\s)aws\s+sts\s+get-session-token\b",
                "aws sts get-session-token.",
            ),
            // Environment dumps (AUD-013 / ruling 14: exact fe1a8a8 rule, then
            // `env_dump_is_false_positive` carves out `env` used as a directory
            // or venv name — `python3 -m venv env`, `ls env`, `rm -rf env`).
            mk(
                "env_dump",
                r"(?:^|\s)(?:env|printenv|set|export\s+-p|compgen\s+-e)\s*(?:$|[|;&])",
                "printing the full environment is denylisted.",
            ),
            // `.env` reads (cat / less / head / tail / redirection). Exact
            // fe1a8a8 rules; `mask_dotenv` spares the `.env.example` etc.
            // templates only.
            mk(
                "dotenv_read",
                r"(?:^|\s)(?:cat|less|more|head|tail|bat)\s+(?:[^|;&\s]*\.env(?:\.[A-Za-z0-9_\-]+)?)(?:\s|$|[|;&])",
                "reading .env files is denylisted by default.",
            ),
            mk(
                "dotenv_redirect_read",
                r"<\s*[^|;&\s]*\.env(?:\.[A-Za-z0-9_\-]+)?\b",
                ".env redirected onto stdin is denylisted by default.",
            ),
            // Browser cookie/login databases (AUD-013 / ruling 14: exact
            // fe1a8a8 rule, then `cookie_is_false_positive` carves out source
            // files whose name merely contains "cookie" — `src/lib/cookies.ts`,
            // `cargo test cookies`).
            mk(
                "browser_cookie_db",
                r#"(?i)Cookies(?:[/.]|$)|Login Data|key4\.db|cookies\.sqlite"#,
                "browser cookie/login store access.",
            ),
            // ---- Windows / PowerShell / cmd (all case-insensitive) ----
            // System-level destruction and security settings.
            mk(
                "disk_format_windows",
                r#"(?i)(?:^|[\s;&|("'\\/])(?:format(?:\.com)?\s+[a-z]:|diskpart(?:\.exe)?\b)|\b(?:format-volume|clear-disk|remove-partition|initialize-disk)\b"#,
                "formatting or repartitioning a disk is denylisted.",
            ),
            mk(
                "registry_delete",
                r#"(?i)(?:^|[\s;&|("'\\/])reg(?:\.exe)?\s+delete\b|\b(?:remove-itemproperty|remove-item|ri|rm|del|erase|rd|rmdir|rp)\b[^|;&]*(?:\bhk(?:lm|cu|cr|u|cc):|registry::)"#,
                "deleting registry keys or values is denylisted.",
            ),
            mk(
                "set_execution_policy",
                r"(?i)\bset-executionpolicy\b",
                "changing the PowerShell execution policy is denylisted.",
            ),
            mk(
                "shadow_copy_delete",
                r#"(?i)(?:^|[\s;&|("'\\/])(?:vssadmin|wbadmin)(?:\.exe)?\s+delete\b|\bwmic\b[^|;&]*\bshadowcopy\b[^|;&]*\bdelete\b"#,
                "deleting shadow copies or backups is denylisted.",
            ),
            mk(
                "bcdedit_modify",
                r#"(?i)(?:^|[\s;&|("'\\/])bcdedit(?:\.exe)?\b[^|;&]*\s/(?:set|delete|deletevalue|create|copy|import|bootsequence|default|timeout|displayorder|toolsdisplayorder|debug|bootdebug|ems|bootems|dbgsettings|emssettings|hypervisorsettings|store|createstore|sysstore)\b"#,
                "modifying the boot configuration is denylisted.",
            ),
            // Script hosts that fetch and run remote code by themselves.
            mk(
                "remote_script_host",
                r#"(?i)(?:^|[\s;&|("'\\/])(?:mshta(?:\.exe)?\b[^|;&\n]*[a-z][a-z0-9+.\-]*://|regsvr32(?:\.exe)?\b[^|;&\n]*[/-]i:["']?[a-z][a-z0-9+.\-]*://)"#,
                "mshta or regsvr32 /i: running a remote script is denylisted.",
            ),
            // Credential extraction.
            mk(
                "windows_credential_manager",
                r#"(?i)(?:^|[\s;&|("'\\/])(?:cmdkey|vaultcmd|mimikatz)(?:\.exe)?\b"#,
                "Windows credential store access.",
            ),
            mk(
                "win_ssh_keys",
                &format!(r#"(?i){WIN_HOME}[\\/]\.ssh(?:[\\/"'\s)]|$)"#),
                "access to the .ssh folder.",
            ),
            mk(
                "win_aws_credentials",
                &format!(r#"(?i){WIN_HOME}[\\/]\.aws(?:[\\/"'\s)]|$)"#),
                "access to the .aws folder.",
            ),
            mk(
                "win_cloud_credentials",
                &format!(
                    r#"(?i)(?:{WIN_HOME}[\\/](?:\.config[\\/]gcloud|\.kube|\.azure|\.docker|appdata[\\/]roaming[\\/](?:gcloud|postgresql))|(?:\$\{{?env:appdata\}}?|%appdata%)[\\/](?:gcloud|postgresql))(?:[\\/"'\s)]|$)"#
                ),
                "access to cloud/kubernetes/docker/postgres credential folders.",
            ),
            mk(
                "win_netrc_pgpass",
                &format!(r#"(?i){WIN_HOME}[\\/](?:\.netrc|_netrc|\.pgpass)\b"#),
                "access to .netrc / .pgpass.",
            ),
            mk(
                "ps_dotenv_read",
                r#"(?i)(?:^|[\s;&|("'\\/])(?:get-content|gc|type|cat|more|select-string|sls)\b[^|;&]*\.env(?:\.[a-z0-9_\-]+)?(?:["'\s)]|$)"#,
                "reading .env files is denylisted by default.",
            ),
            mk(
                "ps_env_dump",
                r#"(?i)(?:^|[\s;&|("'\\/])(?:get-childitem|gci|dir|ls|get-item|gi)\s+(?:-(?:literal)?path\s+)?env:\\?\*?\s*(?:$|[|;&)])|\[environment\]::getenvironmentvariables"#,
                "printing the full environment is denylisted.",
            ),
            mk(
                "windows_browser_profile",
                &format!("(?i){WIN_BROWSER_PROFILE}"),
                "browser profile access.",
            ),
        ]
    })
}

/// One path component of an `rm` target, and what may follow the last one.
const SEG: &str = r#"[^/\s;&|)"'*]+"#;
const ANY_TAIL: &str = r"(?:/\*?)?";

/// Windows home-directory spellings, as the start of a path: `~`, `$HOME`,
/// `$env:USERPROFILE`, `${env:USERPROFILE}`, `%USERPROFILE%`, `C:\Users\<name>`.
const WIN_HOME: &str = r#"(?:~|\$home|\$\{?env:(?:userprofile|homepath)\}?|%(?:userprofile|homepath)%|[a-z]:[\\/]users[\\/][^\\/\s"']+)"#;

/// Browser profile folders on Windows: Chromium-based `...\User Data`, Firefox
/// `Profiles`.
const WIN_BROWSER_PROFILE: &str = r"(?:(?:google[\\/]chrome(?: beta| sxs)?|chromium|microsoft[\\/]edge|bravesoftware[\\/]brave-browser|vivaldi|opera software[\\/]opera stable)[\\/]+user data|mozilla[\\/]firefox[\\/]profiles)";

/// Browser profile folders on macOS (`Library/Application Support/<vendor>`,
/// a space may be written `\ `) and Linux (`~/.config/<browser>`, also under
/// Flatpak's `~/.var/app/<id>/config`; `~/.mozilla/firefox`; Snap Chromium).
const POSIX_BROWSER_PROFILE: &str = r#"(?:library[\\/]application\\? support[\\/](?:google|chromium|bravesoftware|microsoft\\? edge|vivaldi|com\.operasoftware\.|arc|yandex|firefox)|(?:\.config|\.var[\\/]app[\\/][^\\/\s"']+[\\/]config)[\\/](?:google-chrome|chromium|bravesoftware|microsoft-edge|vivaldi|opera|yandex-browser)|\.mozilla[\\/]firefox|snap[\\/]chromium)"#;

/// Rules that see the text through `mask_dotenv`.
const DOTENV_RULES: &[&str] = &["dotenv_read", "dotenv_redirect_read", "ps_dotenv_read"];

/// `.env` templates and anything that is, or may expand to, a `.env`.
fn dotenv_res() -> (&'static Regex, &'static Regex) {
    static TMPL: OnceLock<Regex> = OnceLock::new();
    static ANY: OnceLock<Regex> = OnceLock::new();
    let tmpl = TMPL.get_or_init(|| {
        Regex::new(r"(?i)\.env\.(?:example|sample|template|dist)")
            .expect("dotenv template regex compiles")
    });
    // A `.env`, or a glob that may expand to one (`.en?`, `.e*`, `.*`).
    let any = ANY.get_or_init(|| {
        Regex::new(r"(?i)\.en(?:v|[?*\[])|\.e[?*\[]|\.[?*\[]").expect("dotenv regex compiles")
    });
    (tmpl, any)
}

/// The text with the committed templates `.env.example`, `.env.sample`,
/// `.env.template`, `.env.dist` spelled `_env...`; `None` if it has none.
fn blank_templates(text: &str) -> Option<String> {
    let (tmpl, _) = dotenv_res();
    let dots: Vec<usize> = tmpl
        .find_iter(text)
        .filter(|m| {
            // `.env.examples`, `.env.example.local` are not templates.
            !text[m.end()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        })
        .map(|m| m.start())
        .collect();
    if dots.is_empty() {
        return None;
    }
    let mut masked = text.to_string();
    for &d in &dots {
        masked.replace_range(d..d + 1, "_");
    }
    Some(masked)
}

/// Does the text name a `.env` other than a template, or a glob that may
/// expand to one?
fn names_real_dotenv(text: &str) -> bool {
    let (_, any) = dotenv_res();
    any.is_match(text) && blank_templates(text).is_none_or(|m| any.is_match(&m))
}

/// The text as the `.env` read rules see it: the committed templates are
/// spelled `_env...`, so they are not reads of a secrets file (AUD-013 /
/// ruling 14 — the only `.env` exception). Everything else, including
/// `app.env`, is left exactly as the base rules saw it. Ruling 17: any other
/// `.env` anywhere in the text — another stage, after a `>&` redirect, a glob
/// — or an expansion keeps every template visible (G3, R1).
fn mask_dotenv(text: &str) -> Cow<'_, str> {
    let (_, any) = dotenv_res();
    if !any.is_match(text) || has_expansion(text) {
        return Cow::Borrowed(text);
    }
    match blank_templates(text) {
        Some(masked) if !any.is_match(&masked) => Cow::Owned(masked),
        _ => Cow::Borrowed(text),
    }
}

/// A parameter, command or process substitution, or a `%VAR%`, whose value the
/// exceptions cannot see: it may name a `.env`, a store, or `env` itself.
fn has_expansion(text: &str) -> bool {
    static PCT: OnceLock<Regex> = OnceLock::new();
    let pct = PCT.get_or_init(|| Regex::new(r"%[A-Za-z_]\w*%").expect("%VAR% regex compiles"));
    text.contains('$')
        || text.contains('`')
        || text.contains("<(")
        || text.contains(">(")
        || text.contains("<<<")
        || pct.is_match(text)
}

/// The whole top-level command, against which each AUD-013 exception is
/// decided (ruling 17): a stage, or the text inside a wrapper, is excused only
/// when the whole command is innocuous under that exception, so a dangerous
/// read in another stage keeps the command blocked, as at base. Each answer is
/// computed once, and only when a rule needs it.
pub(super) struct Whole<'a> {
    text: &'a str,
    env: OnceCell<bool>,
    cookie: OnceCell<bool>,
    dotenv: OnceCell<bool>,
}

impl<'a> Whole<'a> {
    fn new(text: &'a str) -> Self {
        Whole {
            text,
            env: OnceCell::new(),
            cookie: OnceCell::new(),
            dotenv: OnceCell::new(),
        }
    }

    fn env_is_name(&self) -> bool {
        *self.env.get_or_init(|| env_exempt(self.text))
    }

    fn cookie_is_source(&self) -> bool {
        *self.cookie.get_or_init(|| cookie_exempt(self.text))
    }

    fn dotenv_is_template(&self) -> bool {
        *self.dotenv.get_or_init(|| dotenv_exempt(self.text))
    }
}

// Each exception, decided for one text. It never excuses a text that also
// names a real `.env` or browser store, even through a command base would not
// block on its own (`cat .env.example; cp Cookies /tmp`, `ls env; cp .env x`),
// or that runs a shell, whose input the exceptions cannot see.

fn env_exempt(text: &str) -> bool {
    env_dump_is_false_positive(text)
        && !has_shell_word(text)
        && !names_real_dotenv(text)
        && !store_in_reach(text)
}

fn cookie_exempt(text: &str) -> bool {
    cookie_is_false_positive(text) && !has_shell_word(text) && !names_real_dotenv(text)
}

fn dotenv_exempt(text: &str) -> bool {
    matches!(mask_dotenv(text), Cow::Owned(_)) && !has_shell_word(text) && !store_in_reach(text)
}

/// A shell or `eval` anywhere, also as a flag value (`rg --pre=bash`), or
/// `start` as the command name (`start "..."`, not `npm start`). Names are
/// compared case-insensitively, as PowerShell and cmd do.
fn has_shell_word(text: &str) -> bool {
    split_stages_with(text, true).iter().any(|stage| {
        let toks = command_tokens(stage);
        toks.first().is_some_and(|t| norm_cmd(&t.text) == "start")
            || toks.iter().any(|t| {
                let word = t.text.rsplit('=').next().unwrap_or("");
                SHELLS.contains(&norm_cmd(word).as_str())
            })
    })
}

// ---------------------------------------------------------------------------
// AUD-013 exceptions on top of the exact base rules (ruling 14)
// ---------------------------------------------------------------------------

/// The base `env_dump` pattern, to re-test a single stage.
fn env_dump_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?:^|\s)(?:env|printenv|set|export\s+-p|compgen\s+-e)\s*(?:$|[|;&])")
            .expect("env_dump regex compiles")
    })
}

/// Exact browser-store filenames (case-sensitive): the real stores, so a
/// lower-case `cookies` source file or a `cargo test cookies` filter is not one.
const STORE_BASENAMES: &[&str] = &[
    "Cookies",
    "Cookies-journal",
    "Cookies.binarycookies",
    "Login Data",
    "Login Data-journal",
    "Web Data",
    "key4.db",
    "key3.db",
    "logins.json",
    "cookies.sqlite",
];

/// Commands whose trailing operand is a path or a name, never run — so a
/// trailing `env`/`set` after one is a directory or venv name, not a dump
/// (AUD-013: `python3 -m venv env`, `ls env`, `rm -rf env`, `cd env`,
/// `echo set`). Runners (`docker exec`, `ssh`, `make`, `npm run`, ...) are not
/// here, so `docker exec c env` stays a dump, matching base.
const INNOCUOUS_ENV_ARG_CMDS: &[&str] = &[
    "ls",
    "dir",
    "rm",
    "rmdir",
    "unlink",
    "cd",
    "pushd",
    "popd",
    "mkdir",
    "cp",
    "mv",
    "cat",
    "bat",
    "tac",
    "du",
    "stat",
    "file",
    "touch",
    "chmod",
    "chown",
    "chgrp",
    "ln",
    "tree",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "pwd",
    "which",
    "whereis",
    "type",
    "test",
    "echo",
    "printf",
    "source",
    "tar",
    "zip",
    "unzip",
    "rsync",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "nano",
    "vi",
    "vim",
    "nvim",
    "code",
    "open",
    "virtualenv",
    "pyvenv",
    "mkvirtualenv",
];

/// Lower-cased basename of a path token, stripping quotes, a trailing `/`, and
/// a `name=@` / `@` prefix (`curl -F f=@Cookies`).
fn clean_basename(raw: &str) -> String {
    let mut s = raw.trim_matches(|c| matches!(c, '"' | '\'' | '(' | ')'));
    // `curl -F f=@Cookies`, `mdfind kMDItemFSName=Cookies`: the store name is
    // after a `@` or a `key=`.
    if let Some(at) = s.rfind('@') {
        s = &s[at + 1..];
    }
    if let Some(eq) = s.rfind('=') {
        s = &s[eq + 1..];
    }
    s.rsplit(['/', '\\'])
        .find(|c| !c.is_empty())
        .unwrap_or("")
        .trim_matches(|c| matches!(c, '"' | '\'' | ',' | ';'))
        .to_string()
}

/// Is the trailing `env`/`set`/... in this stage a directory or venv name,
/// not a command that is run?
fn env_stage_is_innocuous(stage: &str) -> bool {
    let head = skip_exec_prefixes(command_tokens(stage));
    let Some(first) = head.first() else {
        return false;
    };
    let cmd = norm_cmd(&first.text);
    // The word as the command itself (`env`, `nohup env`, `docker exec … env`
    // where `env` is run) is a real dump.
    if matches!(
        cmd.as_str(),
        "env" | "printenv" | "set" | "export" | "compgen"
    ) {
        return false;
    }
    let args = &head[1..];
    match cmd.as_str() {
        // `git diff -- env`, `git add env`: a pathspec; but `git submodule
        // foreach env` / `git bisect run env` / `git rebase -x env` run env.
        "git" => !args.iter().any(|t| {
            matches!(t.text.as_str(), "foreach" | "-x" | "--exec")
                || (t.text == "run" && args.iter().any(|u| u.text == "bisect"))
        }),
        // `python -m venv env`, `uv venv env`: the venv directory.
        "python" | "python2" | "python3" | "uv" => args.iter().any(|t| t.text == "venv"),
        other => INNOCUOUS_ENV_ARG_CMDS.contains(&other),
    }
}

/// Commands that only transform their stdin, so an innocuous `env`-as-name
/// stage piped into one is still innocuous (`ls env | wc -l`). Anything else
/// (a shell, a runner) could execute the name, so the exception fails closed.
/// Not `sed` (`e` runs commands), `awk` (`system`) or `tee` (writes a file).
const STDIN_FILTERS: &[&str] = &[
    "head", "tail", "grep", "egrep", "fgrep", "rg", "ag", "less", "more", "wc", "sort", "uniq",
    "cut", "tr", "column", "nl", "cat", "xxd", "hexdump", "rev", "tac",
];

/// Words that run text as a command. Anywhere in the command they void every
/// AUD-013 exception: `echo env | tr a-z a-z | sh`, `ls env | rg --pre=bash .`,
/// `x=env; ls env; eval $x`. The PowerShell wrappers `iex`/`Invoke-Expression`
/// and `Start-Process`/`saps` run their quoted argument too (F1).
const SHELLS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "csh",
    "tcsh",
    "pwsh",
    "powershell",
    "cmd",
    "eval",
    "iex",
    "invoke-expression",
    "start-process",
    "saps",
];

/// Commands that print the environment, as a command or a name.
const ENV_WORDS: &[&str] = &[
    "env", "printenv", "set", "export", "declare", "typeset", "compgen",
];

/// True when every stage of `text` that the base `env_dump` rule matches is an
/// innocuous `env`-as-name use and nothing else in the command could run the
/// name, so the match is a false positive (ruling 17). Fails closed on a
/// substitution or expansion (G2), an output redirect (the name written to a
/// script), and a pipe into anything but a pure filter; `env_exempt` adds the
/// shell words.
fn env_dump_is_false_positive(text: &str) -> bool {
    if has_expansion(text) || text.contains('>') {
        return false;
    }
    let stages = split_stages_piped(text, true);
    let mut any = false;
    for (i, (stage, _)) in stages.iter().enumerate() {
        let matched = env_dump_re().is_match(stage);
        any |= matched;
        // A stage that base did not match but that dumps the environment
        // another way (`env -0`, `declare -x`, `/proc/self/environ`) is not
        // innocuous either.
        let environ = stage.contains("environ");
        let env_word = command_tokens(stage)
            .iter()
            .any(|t| ENV_WORDS.contains(&norm_cmd(&t.text).as_str()));
        if !matched && !env_word && !environ {
            continue;
        }
        if environ || !env_stage_is_innocuous(stage) {
            return false;
        }
        // The name flows down the whole pipe chain; every stage it reaches
        // must be a pure stdin filter (`ls env | grep foo | wc -l`).
        let mut j = i + 1;
        while let Some((next, true)) = stages.get(j) {
            let filter = command_tokens(next)
                .first()
                .is_some_and(|t| STDIN_FILTERS.contains(&norm_cmd(&t.text).as_str()));
            if !filter {
                return false;
            }
            j += 1;
        }
    }
    any
}

/// Does this stage reference a real browser store — an exact store filename, a
/// store inside a browser profile folder, or Safari's `Library/Cookies`?
fn stage_has_real_store(stage: &str) -> bool {
    static PROFILE: OnceLock<Regex> = OnceLock::new();
    let profile = PROFILE.get_or_init(|| {
        Regex::new(&format!(
            r#"(?i)(?:(?:{POSIX_BROWSER_PROFILE}|{WIN_BROWSER_PROFILE})[^|;&\n]*?(?:cookies|login\\? data|key4\.db|logins\.json|web\\? data)|library[\\/]cookies)"#
        ))
        .expect("profile store regex compiles")
    });
    // `Login\ Data`: the escaped space splits the token.
    if profile.is_match(stage) || stage.to_ascii_lowercase().contains("login\\ data") {
        return true;
    }
    let toks = command_tokens(stage);
    // `cargo test cookies`, `npm test -- cookies`: the words are test-name
    // filters, not paths (AUD-013).
    let test_filter = {
        let head = skip_exec_prefixes(toks.clone());
        head.len() > 1
            && matches!(norm_cmd(&head[0].text).as_str(), "cargo" | "npm")
            && head[1].text == "test"
    };
    // A file finder with a case-insensitive flag (`find -iname cookies`,
    // `locate -i cookies`) matches the lower-case store name too.
    let finder = toks
        .first()
        .map(|t| norm_cmd(&t.text))
        .filter(|c| matches!(c.as_str(), "find" | "fd" | "fdfind" | "locate" | "mdfind"));
    // `mdfind` is always case-insensitive; `find -iname`, `locate -i` /
    // `--ignore-case` ask for it (G4).
    let finder_ci = finder.as_deref() == Some("mdfind")
        || (finder.is_some()
            && toks.iter().any(|t| {
                matches!(
                    t.text.as_str(),
                    "-iname" | "-ipath" | "-iwholename" | "-i" | "--ignore-case"
                )
            }));
    for t in &toks {
        let base = clean_basename(&t.text);
        if STORE_BASENAMES.contains(&base.as_str()) {
            return true;
        }
        if finder_ci {
            let b = base.trim_end_matches('*').to_ascii_lowercase();
            if STORE_BASENAMES.iter().any(|s| s.to_ascii_lowercase() == b) {
                return true;
            }
        }
        // Ruling 17: a bare name in the working directory (`cat cookies`,
        // `cat ./cookies`) may be the store itself, since macOS and Windows
        // file systems are case-insensitive; an unquoted glob (`Cook*`) may
        // expand to it.
        if !test_filter
            && (is_bare_store_name(&t.text) || (!t.quoted && t.text.contains(['*', '?', '['])))
        {
            return true;
        }
        let lower = t.text.to_ascii_lowercase();
        if [
            "login data",
            "key4.db",
            "key3.db",
            "cookies.sqlite",
            "logins.json",
        ]
        .iter()
        .any(|s| lower.contains(s))
        {
            return true;
        }
    }
    false
}

/// A token naming a store file in the working directory, in any letter case:
/// `cookies`, `./Cookies`, `'login data'` (the `@`/`key=` prefixes as in
/// `clean_basename`).
fn is_bare_store_name(raw: &str) -> bool {
    let mut s = raw.trim_matches(|c| matches!(c, '"' | '\'' | '(' | ')'));
    if let Some(at) = s.rfind('@') {
        s = &s[at + 1..];
    }
    if let Some(eq) = s.rfind('=') {
        s = &s[eq + 1..];
    }
    while let Some(rest) = s.strip_prefix("./").or_else(|| s.strip_prefix(".\\")) {
        s = rest;
    }
    let s = s.trim_end_matches(['/', '\\']);
    !s.contains(['/', '\\']) && STORE_BASENAMES.iter().any(|b| b.eq_ignore_ascii_case(s))
}

/// True when every stage of `text` that the base `browser_cookie_db` rule
/// matches has no real store reference, so the match is only a source/text
/// file whose name contains "cookie" (AUD-013: `cat src/lib/cookies.ts`,
/// `cargo test cookies`). Parentheses are kept inside their token.
fn cookie_is_false_positive(text: &str) -> bool {
    // A store read anywhere blocks the whole command (G4): a browser-profile
    // folder (`cd <profile> && cat cookies`) or a real store in any stage.
    // An expansion may name a store (`cat src/cookies.ts $F`).
    if has_expansion(text) || store_in_reach(text) {
        return false;
    }
    split_stages_with(text, false)
        .iter()
        .any(|s| cookie_re().is_match(s))
}

/// A browser-profile folder anywhere, or a real store in any stage.
fn store_in_reach(text: &str) -> bool {
    browser_profile_anywhere().is_match(text)
        || split_stages_with(text, false)
            .iter()
            .any(|s| stage_has_real_store(s))
}

/// A browser-profile folder (macOS, Linux or Windows) appearing anywhere.
fn browser_profile_anywhere() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(&format!(
            "(?i)(?:{POSIX_BROWSER_PROFILE}|{WIN_BROWSER_PROFILE})"
        ))
        .expect("browser profile regex compiles")
    })
}

/// The base `browser_cookie_db` pattern, to re-test a single stage.
fn cookie_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?i)Cookies(?:[/.]|$)|Login Data|key4\.db|cookies\.sqlite"#)
            .expect("cookie regex compiles")
    })
}

/// Record a block hit once per rule: the same rule found at several unwrap
/// levels is one hit.
pub(super) fn push_hit(hits: &mut Vec<RuleHit>, hit: RuleHit) {
    if !hits.iter().any(|h| h.rule_name == hit.rule_name) {
        hits.push(hit);
    }
}

fn apply_regex_block_rules(
    stage: &str,
    original: &str,
    hits: &mut Vec<RuleHit>,
    depth: usize,
    whole: &Whole,
) {
    // An exception holds only when both the whole command and this text are
    // innocuous under it (ruling 17). The text matters inside a wrapper, where
    // it may be decoded from or quoted in text the whole command does not
    // show as tokens.
    let dotenv = match mask_dotenv(stage) {
        Cow::Owned(m)
            if whole.dotenv_is_template() && !has_shell_word(stage) && !store_in_reach(stage) =>
        {
            Cow::Owned(m)
        }
        _ => Cow::Borrowed(stage),
    };
    for (name, re, note) in block_rules() {
        let text: &str = if DOTENV_RULES.contains(name) {
            &dotenv
        } else {
            stage
        };
        if re.is_match(text) {
            // Exact base match, then the narrow AUD-013 exceptions (ruling 14):
            // `env` used as a directory/venv name, and source files whose name
            // merely contains "cookie". The `env`-as-name exception applies
            // only at depth 0: inside `$(...)` or a wrapper, `env`/`set` is a
            // command being executed, not a path operand (G2). At depth 0 the
            // text is the whole command or one of its stages, so the whole
            // command's answer covers it.
            if *name == "env_dump" && depth == 0 && whole.env_is_name() {
                continue;
            }
            if *name == "browser_cookie_db" && whole.cookie_is_source() && cookie_exempt(stage) {
                continue;
            }
            push_hit(
                hits,
                RuleHit {
                    rule_name: name,
                    stage: original.to_string(),
                    note: note.to_string(),
                },
            );
        }
    }
}

/// `depth` counts how many wrappers and `$(...)` the text is inside. The text
/// at `MAX_UNWRAP_DEPTH` is still examined, but nothing inside it is unwrapped
/// (the same limit `infer_stage_risk` uses).
fn apply_block_rules(stage: &str, hits: &mut Vec<RuleHit>, depth: usize, whole: &Whole) {
    #[cfg(test)]
    RULE_CALLS.with(|c| c.set(c.get() + 1));
    apply_regex_block_rules(stage, stage, hits, depth, whole);
    // The same stage with its command name normalised, so quotes, a call
    // operator or a directory in front of it do not hide it:
    // `"reg" delete`, `& "C:\Windows\System32\reg.exe" delete`.
    if let Some(canon) = canonical_stage(stage) {
        apply_regex_block_rules(&canon, stage, hits, depth, whole);
    }
    // Token rules see parenthesised arguments in place (`Remove-Item -Recurse
    // ($env:USERPROFILE)`), so they run on a splitter that keeps `(` `)`.
    for part in split_stages_with(stage, false) {
        apply_windows_block_rules(&part, hits, depth, whole);
    }
    // The download rule follows wrappers and `$(...)` itself.
    if depth == 0 {
        download::apply(stage, hits);
    }
    // What the shell runs when it expands the string: `"$(rm -rf ~)"`.
    if depth < MAX_UNWRAP_DEPTH {
        for sub in subexpressions(stage) {
            apply_block_rules(&sub, hits, depth + 1, whole);
        }
    }
}

// ---------------------------------------------------------------------------
// Warn rules — non-fatal but worth flagging
// ---------------------------------------------------------------------------

fn warn_rules() -> &'static [(&'static str, Regex, &'static str)] {
    static R: OnceLock<Vec<(&'static str, Regex, &'static str)>> = OnceLock::new();
    R.get_or_init(|| {
        let mk = |name: &'static str, pat: &str, note: &'static str| {
            (name, Regex::new(pat).expect("warn regex compiles"), note)
        };
        vec![
            mk(
                "rm_rf_under_cwd",
                r"(?:^|\s)rm\s+(?:-[a-zA-Z]*[rR][a-zA-Z]*[fF]?|-[a-zA-Z]*[fF][a-zA-Z]*[rR])\b",
                "recursive removal — confirm target carefully.",
            ),
            mk(
                "background_amp",
                r"&\s*$",
                "command would background with `&`.",
            ),
            mk(
                "write_outside_cwd",
                r">\s*(?:/|~)",
                "redirecting output outside the project root.",
            ),
            mk(
                "network_egress",
                r"(?:^|\s)(?:curl|wget|nc|ssh|scp|rsync)\b",
                "command performs network egress.",
            ),
            mk(
                "ps_recursive_remove",
                r"(?i)(?:^|\s)(?:remove-item|ri|rm|del|erase|rd|rmdir)\b[^|;&]*\s(?:-recurse|-r|/s)\b",
                "recursive removal — confirm target carefully.",
            ),
            mk(
                "ps_network_egress",
                r"(?i)(?:^|\s)(?:iwr|irm|invoke-webrequest|invoke-restmethod|test-netconnection|start-bitstransfer)\b",
                "command performs network egress.",
            ),
            mk(
                "long_watcher",
                r"(?:^|\s)(?:tail\s+-f|watch\b|npm\s+run\s+dev|yarn\s+dev)\b",
                "long-running watcher — likely to hit the timeout.",
            ),
        ]
    })
}

fn apply_warn_rules(stage: &str, hits: &mut Vec<RuleHit>) {
    for (name, re, note) in warn_rules() {
        if re.is_match(stage) {
            hits.push(RuleHit {
                rule_name: name,
                stage: stage.to_string(),
                note: note.to_string(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Stage tokenisation
// ---------------------------------------------------------------------------

/// Split on shell stage delimiters: `|`, `||`, `&&`, `;`, a single `&`
/// between statements, newlines, and subshell boundaries (`(`, `)`, `$(`).
/// Quoting is honored at a very basic level so a literal `|` inside `"…"` or
/// `'…'` is not split on. A backtick-escaped quote (`` `" ``) stays inside a
/// double-quoted string; a backslash never escapes (`"C:\"` is a closed string).
/// A trailing `&` (background), a PowerShell call operator at the start of a
/// stage (`& "x.exe"`) and redirections (`2>&1`, `&>`) stay on the stage.
fn split_stages(command: &str) -> Vec<String> {
    split_stages_with(command, true)
}

/// `split_parens: false` keeps parentheses inside their stage.
fn split_stages_with(command: &str, split_parens: bool) -> Vec<String> {
    split_stages_piped(command, split_parens)
        .into_iter()
        .map(|(stage, _)| stage)
        .collect()
}

/// Like `split_stages_with`; the flag says the stage reads the output of the
/// previous one (it follows a single `|`).
pub(super) fn split_stages_piped(command: &str, split_parens: bool) -> Vec<(String, bool)> {
    let mut stages: Vec<(String, bool)> = Vec::new();
    let mut next_piped = false;
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        if in_double {
            if c == '`' && i + 1 < n && chars[i + 1] == '"' {
                cur.push(c);
                cur.push('"');
                i += 2;
                continue;
            }
            if c == '"' {
                in_double = false;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                cur.push(c);
                i += 1;
            }
            '"' => {
                in_double = true;
                cur.push(c);
                i += 1;
            }
            '|' | ';' => {
                push_stage(&mut stages, &mut cur, &mut next_piped);
                // Skip a second delimiter char (||).
                i += 1;
                if i < n && chars[i] == c {
                    i += 1;
                } else {
                    next_piped = c == '|';
                }
            }
            '\n' | '\r' => {
                push_stage(&mut stages, &mut cur, &mut next_piped);
                i += 1;
            }
            '&' => {
                if i + 1 < n && chars[i + 1] == '&' {
                    push_stage(&mut stages, &mut cur, &mut next_piped);
                    i += 2;
                } else {
                    let after = chars[i + 1..].iter().find(|ch| **ch != ' ' && **ch != '\t');
                    let trailing = after.is_none_or(|ch| *ch == '\n' || *ch == '\r');
                    let redirect = matches!(cur.chars().last(), Some('>' | '<'))
                        || chars.get(i + 1) == Some(&'>');
                    if trailing || redirect || cur.trim().is_empty() {
                        cur.push(c);
                    } else {
                        push_stage(&mut stages, &mut cur, &mut next_piped);
                    }
                    i += 1;
                }
            }
            '(' | ')' if split_parens => {
                push_stage(&mut stages, &mut cur, &mut next_piped);
                i += 1;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    push_stage(&mut stages, &mut cur, &mut next_piped);
    stages
}

fn push_stage(stages: &mut Vec<(String, bool)>, cur: &mut String, next_piped: &mut bool) {
    let s = cur.trim().to_string();
    if !s.is_empty() {
        stages.push((s, *next_piped));
        *next_piped = false;
    }
    cur.clear();
}

#[cfg(test)]
mod autorun_tests;
#[cfg(test)]
mod tests;
