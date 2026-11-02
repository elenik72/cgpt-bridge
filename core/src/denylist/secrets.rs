//! Known places that hold secrets (B6, user decision 7 of 28.10).
//!
//! A command whose words name one of these places is a secrets read,
//! whatever program reads it and however: the classifier never marks it
//! `ReadOnly` and adds the `secret_location` warning. Only the words written
//! in the command are seen, not paths built at run time or found by the
//! program itself. A word that only excludes a place from a grep, rg or find
//! search (Ruling 47, see `excluded_words`) does not name it.
//!
//! The list is matched on path components, so a folder covers everything
//! under it. Names are compared ignoring case, as the default macOS and
//! Windows file systems do; only Chromium's `Cookies` store is matched with
//! its case, so that a project folder or file named `cookies` is not taken
//! for it.

use super::shell::{norm_cmd, tokenize, Tok};
use super::{split_stages, MAX_UNWRAP_DEPTH};

/// How one entry of [`SECRET_LOCATIONS`] matches the components of a path
/// (split at `/` and `\`; lower-cased except for `CaseFile`). Every entry
/// but `CaseFile` must be written in lower case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretLocation {
    /// A component with exactly this name: `.ssh` matches `~/.ssh`,
    /// `.ssh/config` and `x/.ssh/id`.
    Name(&'static str),
    /// The last component (the file name) is exactly this, ignoring case.
    File(&'static str),
    /// The last component is exactly this, case included.
    CaseFile(&'static str),
    /// A component starting with this text: `.env.` matches `.env.local`.
    Prefix(&'static str),
    /// A component ending with this text: `.pem` matches `server.pem`.
    Suffix(&'static str),
    /// These components one after another anywhere in the path; `*` stands
    /// for any one component.
    Run(&'static [&'static str]),
}

/// Known places that hold secrets. The classifier's `secret_location` rule
/// uses it through [`names_secret_location`]; other checks of secret paths
/// (such as the `--auto-readonly` list) should call that function rather
/// than keep a copy.
///
/// `.env` and `.env.*` count, except the committed templates in
/// [`DOTENV_TEMPLATES`].
pub const SECRET_LOCATIONS: &[SecretLocation] = {
    use SecretLocation::{CaseFile, File, Name, Prefix, Run, Suffix};
    &[
        // Environment files (a `.env` folder is a virtualenv, not one).
        File(".env"),
        Prefix(".env."),
        // SSH keys and configuration; private keys by their default names
        // (the `.pub` halves are public).
        Name(".ssh"),
        File("id_rsa"),
        File("id_dsa"),
        File("id_ecdsa"),
        File("id_ed25519"),
        File("id_ecdsa_sk"),
        File("id_ed25519_sk"),
        // Cloud and package registry credentials.
        Name(".aws"),
        Name(".netrc"),
        Name(".npmrc"),
        Name(".pypirc"),
        File("credentials"),
        File(".credentials"),
        File("credentials.json"),
        File("credentials.yml"),
        File("credentials.yaml"),
        File("credentials.toml"),
        File("credentials.ini"),
        File("credentials.csv"),
        File("credentials.db"),
        // A repository's configuration may hold tokens in remote URLs.
        Name(".git"),
        // Keys and certificates.
        Suffix(".pem"),
        Suffix(".key"),
        Suffix(".p12"),
        Suffix(".pfx"),
        // The environment of a process.
        Run(&["proc", "*", "environ"]),
        // Browser profiles on macOS and Linux.
        Run(&["library", "application support", "google", "chrome"]),
        Run(&["library", "application support", "chromium"]),
        Run(&["library", "application support", "firefox"]),
        Run(&["library", "safari"]),
        Run(&["library", "cookies"]),
        Run(&[".config", "google-chrome"]),
        Run(&[".config", "chromium"]),
        Run(&[".config", "mozilla", "firefox"]),
        Run(&[".mozilla", "firefox"]),
        // Browser login and cookie stores.
        CaseFile("Cookies"),
        File("cookies-journal"),
        File("login data"),
        File("login data-journal"),
        File("web data"),
        File("key4.db"),
        File("logins.json"),
        File("cookies.sqlite"),
        // The macOS keychain.
        Run(&["library", "keychains"]),
    ]
};

/// Committed `.env` templates, which hold no secrets.
pub const DOTENV_TEMPLATES: &[&str] =
    &[".env.example", ".env.sample", ".env.template", ".env.dist"];

/// Does any word of `text` name a place in [`SECRET_LOCATIONS`]? `text` is a
/// command, a stage or a single word. Quotes are removed, a backslash-escaped
/// space joins its word (`Application\ Support`), a flag value
/// (`--file=x`), a list item (`a,b`) and the text after `@` or `:` are
/// separate words, and the words of a quoted command (`bash -c '...'`) are
/// examined too. The text is split into stages first, and each stage's
/// exclusion words (`excluded_words`) apply to that stage only.
pub fn names_secret_location(text: &str) -> bool {
    names_at(text, 0)
}

fn names_at(text: &str, depth: usize) -> bool {
    split_stages(text)
        .iter()
        .any(|stage| stage_names_at(stage, depth))
}

fn stage_names_at(stage: &str, depth: usize) -> bool {
    let joined = stage.replace("\\ ", "\u{1}");
    let toks = tokenize(&joined, false);
    let excluded = excluded_words(&toks);
    toks.iter().zip(excluded).any(|(tok, excluded)| {
        if excluded {
            return false;
        }
        let word = tok.text.replace('\u{1}', " ");
        word.split(['=', ',', '@', ':', ';', '|', '&', '<', '>', '(', ')'])
            .any(path_is_secret)
            || (depth < MAX_UNWRAP_DEPTH
                && tok.text.contains(char::is_whitespace)
                && names_at(&tok.text, depth + 1))
    })
}

/// Words of one stage that only exclude a place from a search (Ruling 47),
/// by the command that the stage's first word names: grep `--exclude=X` and
/// `--exclude-dir=X`, rg `-g '!X'` and `--glob '!X'`, and find's
/// `-path X -prune`, `-not -path X` and `! -path X`. Any other form is a
/// mention. The option position is not checked: a skipped word could be a
/// grep operand after `--` or an rg operand after a `-g` another option
/// consumed, but it then starts with `--exclude` or `!`, so it reaches a
/// secrets location only through a folder of that literal name.
fn excluded_words(toks: &[Tok]) -> Vec<bool> {
    let mut skip = vec![false; toks.len()];
    let Some(first) = toks.first() else {
        return skip;
    };
    let cmd = norm_cmd(&first.text);
    let text = |i: usize| toks.get(i).map(|t| t.text.as_str());
    for i in 1..toks.len() {
        let t = toks[i].text.as_str();
        let value = i + 1 < toks.len();
        match cmd.as_str() {
            "grep" => skip[i] |= t.starts_with("--exclude=") || t.starts_with("--exclude-dir="),
            "rg" if value && matches!(t, "-g" | "--glob") => {
                skip[i + 1] |= text(i + 1).is_some_and(|v| v.starts_with('!'));
            }
            "find" if value && t == "-path" => {
                skip[i + 1] |=
                    text(i + 2) == Some("-prune") || matches!(text(i - 1), Some("-not" | "!"));
            }
            _ => {}
        }
    }
    skip
}

/// Is `path` (one path, not a command) in [`SECRET_LOCATIONS`]? `/` and `\`
/// both separate components, and spaces are part of a component
/// (`Library/Application Support/...`). Nothing is expanded or resolved: `~`,
/// `.` and `..` are components like any other.
pub fn path_is_secret(path: &str) -> bool {
    let file = path.split(['/', '\\']).rfind(|c| !c.is_empty());
    let lower = path.to_lowercase();
    let comps: Vec<&str> = lower.split(['/', '\\']).filter(|c| !c.is_empty()).collect();
    SECRET_LOCATIONS.iter().any(|loc| match *loc {
        SecretLocation::Name(n) => comps.contains(&n),
        SecretLocation::File(f) => comps.last() == Some(&f),
        SecretLocation::CaseFile(f) => file == Some(f),
        SecretLocation::Prefix(p) => comps
            .iter()
            .any(|c| c.starts_with(p) && !DOTENV_TEMPLATES.contains(c)),
        SecretLocation::Suffix(s) => comps.iter().any(|c| c.ends_with(s)),
        SecretLocation::Run(run) => comps
            .windows(run.len())
            .any(|w| w.iter().zip(run).all(|(c, r)| *r == "*" || c == r)),
    })
}
