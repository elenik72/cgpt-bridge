//! Secret-redaction pipeline applied to command output before it is sent
//! back to ChatGPT in a `cgpt-command-result-v1` block.
//!
//! Patterns from `docs/security.md` §7. Conservative on purpose — false
//! positives are preferred over false negatives. A match is replaced with
//! `«redacted:<rule_name>»` and `<rule_name>` is added to the result's
//! `redactions_applied` list.
//!
//! Order matters: longer, more-specific patterns run before shorter ones so
//! we do not partially redact a longer match into a still-sensitive remnant.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Redaction {
    pub stdout: String,
    pub stderr: String,
    pub rules_fired: Vec<String>,
}

struct Rule {
    name: &'static str,
    re: Regex,
    mode: Mode,
    /// For `Mode::Value`: the text of group 2 is replaced only if this holds.
    accept: Option<fn(&str) -> bool>,
}

/// What of a match is replaced.
#[derive(Clone, Copy)]
enum Mode {
    /// The whole match.
    Whole,
    /// Capture group 2 only; groups 1 and 3 (the key, the `@` of a URL) stay.
    Value,
    /// Like `Value`, for `key: value` pairs: quotes around the value stay,
    /// and empty, null, boolean and number values are left alone.
    KeyValue,
    /// `key = value` assignments (see `assignment_replacement`): groups
    /// `keep` (up to the value) and `key`, then the first character of the
    /// value; the value runs to the end of the line.
    Assignment,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(build_rules)
}

fn build_rules() -> Vec<Rule> {
    let mut v = Vec::new();
    let add_with = |v: &mut Vec<Rule>, name: &'static str, pat: &str, mode: Mode| {
        v.push(Rule {
            name,
            re: Regex::new(pat).expect("redaction regex must compile"),
            mode,
            accept: None,
        });
    };
    let add =
        |v: &mut Vec<Rule>, name: &'static str, pat: &str| add_with(v, name, pat, Mode::Whole);

    // Multi-line private key blocks first — these are the loudest signal and
    // we want to redact them whole, not piece by piece.
    add(
        &mut v,
        "private_key_block",
        r"(?s)-----BEGIN (?:RSA |EC |OPENSSH |DSA |ENCRYPTED |)PRIVATE KEY-----.*?-----END (?:RSA |EC |OPENSSH |DSA |ENCRYPTED |)PRIVATE KEY-----",
    );
    // A PEM private key in base64 (kubeconfig `client-key-data`, Kubernetes
    // secrets): the encoded `-----BEGIN ... PRIVATE KEY-----` header.
    add(
        &mut v,
        "private_key_base64",
        r"LS0tLS1CRUdJTiB(?:QUklWQVRFIEtFWS0tLS0t|SU0EgUFJJVkFURSBLRVktLS0t|FQyBQUklWQVRFIEtFWS0tLS0t|EU0EgUFJJVkFURSBLRVktLS0t|PUEVOU1NIIFBSSVZBVEUgS0VZLS0t|FTkNSWVBURUQgUFJJVkFURSBLRVktLS0t)[A-Za-z0-9+/=]*",
    );
    add_with(
        &mut v,
        "kube_client_key_data",
        r"(?im)(client-key-data[ \t]*:[ \t]*)([^\s«]\S*)",
        Mode::Value,
    );
    add(
        &mut v,
        "pgp_private_key_block",
        r"(?s)-----BEGIN PGP PRIVATE KEY BLOCK-----.*?-----END PGP PRIVATE KEY BLOCK-----",
    );

    // Vendor API keys with stable prefixes. `sk-` opens a token only: not
    // inside a word (`desk-booking-service-…`, `task-scheduler-…`). The
    // regex crate has no lookbehind, so what stands before `sk-` is group 1
    // and stays, and the key is group 2. Before the key may stand the text
    // start, a non-word character, a JSON escape written out (`\n`, `\r`,
    // `\t`) or a URL-encoded space, quote, colon, equals or line break
    // (`%20 %22 %27 %3A %3D %0A %0D %09`); not `%2F` and the like, which
    // would be a path segment. `anthropic_api_key` goes first so that
    // `sk-ant-…` gets its own name.
    const KEY_LEFT: &str = r"(^|\\[nrt]|%(?:20|22|27|3[Aa]|3[Dd]|0[9AaDd])|[^A-Za-z0-9_\-])";
    add_with(
        &mut v,
        "anthropic_api_key",
        &format!(r"{KEY_LEFT}(sk-ant-[A-Za-z0-9_\-]{{20,}})"),
        Mode::Value,
    );
    add_with(
        &mut v,
        "openai_api_key",
        &format!(r"{KEY_LEFT}(sk-(?:proj-)?[A-Za-z0-9_\-]{{20,}})"),
        Mode::Value,
    );
    add(&mut v, "github_token_classic", r"ghp_[A-Za-z0-9]{30,}");
    add(
        &mut v,
        "github_token_fine_grained",
        r"github_pat_[A-Za-z0-9_]{20,}",
    );
    add(&mut v, "github_oauth", r"gho_[A-Za-z0-9]{30,}");
    add(&mut v, "github_app", r"(?:ghu|ghs|ghr)_[A-Za-z0-9]{30,}");
    // The AWS, cookie, url and netrc rules are plain ASCII, so their word
    // boundary is an ASCII one (`(?-u:\b)`): a Cyrillic letter next to the
    // text is no word, and the rule stays on the fast engine for text with
    // `«` or non-ASCII letters.
    add(
        &mut v,
        "aws_access_key_id",
        r"(?-u:\b)(?:AKIA|ASIA)[0-9A-Z]{16}(?-u:\b)",
    );
    // GitLab (`glpat-` personal, `gldt-` deploy, `glrt-` runner, ...), Google
    // API and Slack tokens: like `sk-`, they open a token only (`KEY_LEFT`).
    // A routable GitLab token ends in `.<two digits>.<checksum>`; a Slack
    // `xoxd-` cookie is URL-encoded (`%2B`). The body of an `xoxc-`, `xoxe-`
    // or `xoxo-` token starts with a digit, so slugs such as
    // `xoxo-hugs-and-kisses` are none; the others (`a b p r s`) keep any
    // body, as before. No quantifier is nested.
    // A random GitLab body has a digit or a capital; a kebab-case name such
    // as `gldt-service-deployment-tools` does not, and is no token.
    add_with(
        &mut v,
        "gitlab_token",
        &format!(
            r"{KEY_LEFT}(gl(?:pat|dt|rt|ptt|ft|imt|agent|cbt|oas|soat|ffct)-[A-Za-z0-9_\-]{{20,}}(?:\.[0-9]{{2}}\.[A-Za-z0-9]{{8}})?)"
        ),
        Mode::Value,
    );
    v.last_mut().expect("rule was just added").accept = Some(|token| {
        token.split_once('-').is_some_and(|(_, body)| {
            body.chars()
                .any(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        })
    });
    add_with(
        &mut v,
        "google_api_key",
        &format!(r"{KEY_LEFT}(AIza[A-Za-z0-9_\-]{{35,}})"),
        Mode::Value,
    );
    add_with(
        &mut v,
        "slack_token",
        &format!(
            r"{KEY_LEFT}(xox(?:[abprs]-[A-Za-z0-9%_\-]{{10,}}|[ceo]-[0-9][A-Za-z0-9%_\-]{{9,}}|d-[A-Za-z0-9%_\-]{{10,}}))"
        ),
        Mode::Value,
    );
    add(&mut v, "npm_token", r"npm_[A-Za-z0-9]{30,}");
    add(&mut v, "stripe_live_secret", r"sk_live_[A-Za-z0-9]{20,}");

    // JWT-like (three base64url-ish segments separated by dots, starts eyJ).
    add(
        &mut v,
        "jwt_like",
        r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
    );

    // Bearer / Basic auth headers.
    // Also as a JSON or YAML pair (`"Authorization": "Bearer ..."`); the
    // header name and scheme stay.
    add_with(
        &mut v,
        "generic_bearer_token",
        r#"(?i)(Authorization["']?[ \t]*[:=][ \t]*["']?[ \t]*Bearer\s+)([A-Za-z0-9._\-~+/]+=*)"#,
        Mode::Value,
    );
    add_with(
        &mut v,
        "generic_basic_auth",
        r#"(?i)(Authorization["']?[ \t]*[:=][ \t]*["']?[ \t]*Basic\s+)([A-Za-z0-9+/=]+)"#,
        Mode::Value,
    );
    // `Cookie: a=b; c=d`, `"Cookie": "session=..."`, `Set-Cookie: ...`, also
    // in the middle of a line (a log, `curl -H Cookie:x`, `map[Cookie:[x]]`).
    // The value must begin with a cookie pair `name=`, so code such as
    // `cookie: string;` or `let cookie = parse(h)` is not one. An `=` is a
    // separator too: glued, also with a quote (`Cookie=a=b`,
    // `@{Cookie="a=b"}`), or with blanks (`Cookie = a=b`); a spaced `=` and a
    // quote (`document.cookie = "a=b"`) is code, unless the key is a
    // PowerShell hashtable's (`@{ Cookie = "a=b" }`, after `{` or `;`) or a
    // quoted one (`"Cookie" = "a=b"`, not `obj["cookie"]`), or a capitalised
    // key alone on an indented line (a multi-line hashtable).
    add_with(
        &mut v,
        "cookie_header",
        r#"(?im)((?-u:\b)(?:set-)?cookie["']?(?:[ \t]*:[ \t]*["'\[]?|=["']?|[ \t]+=[ \t]*)|(?:[{;][ \t]*["']?|(?:^|[^\[\w])["'])(?:set-)?cookie["']?[ \t]+=[ \t]*["']|^[ \t]+(?-i:(?:Set-)?Cookie)[ \t]+=[ \t]*["'])([^\s"'«=;:,()\[\]{}\x60]+=[^"'\n]*)"#,
        Mode::Value,
    );
    // .npmrc `_auth=<base64 user:pass>`, also scoped `//host/:_auth=`.
    add_with(
        &mut v,
        "npmrc_auth",
        r"(?im)^((?:[^\n]*:)?_auth[ \t]*=[ \t]*)(\S.*)$",
        Mode::Value,
    );

    // `scheme://user:password@host`: the password only. A user that opens
    // with `«` is the marker of an earlier rule (`https://ghp_…@host` is
    // `https://«redacted:github_token_classic»@host` by now): it is a user
    // only as one whole marker, so that `https://<token>:pass@host` still
    // loses its password, and `https://<token>@host` is not `user:`.
    // A password that is such a marker already stays as it is.
    add_with(
        &mut v,
        "url_userinfo_password",
        r"(?i)(?-u:\b)([a-z][a-z0-9+.\-]*://(?:«redacted:[a-z0-9_]+»|[^\s:/@«][^\s:/@]*):)([^\s/@]+)(@)",
        Mode::Value,
    );
    v.last_mut().expect("rule was just added").accept = Some(|password| !is_marker(password));

    // .env-style assignments. We replace only the value, not the key, so the
    // structure remains readable.
    add_with(
        &mut v,
        "dotenv_secret_assignment",
        r"(?m)(?P<keep>(?:^|[^A-Za-z0-9_.\-])(?P<key>[A-Za-z0-9_.\-]+)[ \t]*=[ \t]*)[^\s=>]",
        Mode::Assignment,
    );

    // JSON / YAML / `key: value` pairs whose key is a secret's name (see
    // `is_secret_key`). A quoted key may stand anywhere (`{"apiKey": ...}`),
    // an unquoted one only at the start of a line (YAML, after indentation
    // or `- `), so code such as `f(token: str)` is left alone. Before an
    // unquoted key may also stand what a pasted diff or log puts there
    // (`LINE_LEAD`). An unquoted value is one word; `${VAR}` is one value.
    // The fields of a log line are few and short: at most 6, a bracketed one
    // up to 80 characters. So the lead of a match is short, not the whole
    // of a long line of fields, and the match is found at a linear cost.
    const LINE_LEAD: &str = concat!(
        // A diff marker or a quote mark, then indentation.
        r"^[ \t]*(?:[+>][ \t]*)?",
        // Log fields, each followed by blanks: `[app]`, a date, a time, a level.
        r"(?:(?:\[[^\]\n]{0,80}\]",
        r"|[0-9]{4}-[0-9]{2}-[0-9]{2}(?:T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:[.,][0-9]+)?(?:Z|[+\-][0-9]{2}(?::?[0-9]{2})?)?)?",
        r"|[0-9]{2}:[0-9]{2}:[0-9]{2}(?:[.,][0-9]+)?",
        r"|(?:TRACE|DEBUG|INFO|NOTICE|WARN|WARNING|ERROR|ERR|FATAL|CRITICAL))[ \t]+){0,6}",
        // A list item.
        r"(?:-[ \t]+)?",
    );
    let key_value = r#"(?m)(?P<keep>(?:(?:^|[\s{,\[(])["'](?P<qk>[A-Za-z0-9_.\-/@]+)["']|@LEAD@(?P<bk>[A-Za-z0-9_.\-]+))[ \t]*:[ \t]*)(?P<value>"(?:[^"\\\n]|\\.)*"|'[^'\n]*'|\$?\{\{[^}\n]*\}\}|\$\{[^}\n]*\}|[^\s;),"'{\[\]}][^\s;),"'\]}]*)"#
        .replace("@LEAD@", LINE_LEAD);
    add_with(&mut v, "key_value_secret", &key_value, Mode::KeyValue);

    // .netrc: `machine h login u password p`, `default password p`, or one
    // entry per indented line. A line of prose starting `password ...` is
    // not one.
    add_with(
        &mut v,
        "netrc_password",
        r"(?m)((?:^[ \t]+|(?-u:\b)machine[ \t]+\S+(?:[ \t]+(?:login|account|port)[ \t]+\S+)*[ \t]+|(?-u:\b)default(?:[ \t]+(?:login|account)[ \t]+\S+)*[ \t]+)password[ \t]+)([^\s«]\S*)",
        Mode::Value,
    );

    v
}

/// Words that make a key a secret's name, lower-case; `api key` and
/// `private key` also as two words.
const SECRET_KEY_WORDS: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "passphrase",
    "apikey",
    "privatekey",
];
const SECRET_KEY_PAIRS: &[(&str, &str)] = &[("api", "key"), ("private", "key")];
/// Words that make a key a secret's name only as its last word: a container
/// registry's `"auth": "<base64 user:pass>"`, `registry_auth`. Elsewhere in a
/// key (`authMiddleware`, `auth_provider`) it names code, not a secret; it is
/// never glued (`author`, `oauth_callback`). `auth` is also a common name in
/// code (`auth: authReducer`, `self.auth = auth`), so such a key is a secret's
/// only with a value shaped like base64 (see `auth_value_is_code`).
const SECRET_KEY_LAST_WORDS: &[&str] = &["auth"];

/// Last words of a key that name something about a secret, not the secret:
/// `secretName`, `password_file`, `token_ttl`, `private_key_id`.
const SECRET_META_WORDS: &[&str] = &[
    "name",
    "names",
    "id",
    "ids",
    "arn",
    "ref",
    "refs",
    "type",
    "kind",
    "ttl",
    "length",
    "len",
    "count",
    "size",
    "min",
    "max",
    "path",
    "file",
    "files",
    "dir",
    "url",
    "uri",
    "endpoint",
    "header",
    "env",
    "var",
    "field",
    "version",
    "enabled",
    "required",
    "expiry",
    "expires",
    "expiration",
    "at",
    "format",
    "prefix",
    "hint",
    "policy",
    "mode",
    "label",
    "description",
    "rotation",
];

/// Words of a key, lower-case: split at every character that is not a
/// letter or digit, where lower case turns upper (`secretAccessKey`), and
/// between letters and digits (`password1`).
fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in key.chars() {
        let boundary = !c.is_ascii_alphanumeric()
            || prev.is_some_and(|p| {
                (p.is_ascii_lowercase() && c.is_ascii_uppercase())
                    || (p.is_ascii_alphabetic() != c.is_ascii_alphabetic())
            });
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur).to_ascii_lowercase());
        }
        if c.is_ascii_alphanumeric() {
            cur.push(c);
        }
        prev = Some(c);
    }
    if !cur.is_empty() {
        words.push(cur.to_ascii_lowercase());
    }
    words
}

/// Is this key a secret's name? A secret word is a whole word of it
/// (`client_secret`, `SECRET_KEY_BASE`, `secretAccessKey`, `db_password_prod`,
/// `password1`, `apiKeyValue`, `X-Api-Token`), not glued into another word
/// (`jsonwebtoken`), and the key does not end in a word about the secret
/// (`secretName`, `token_count`). Values such as types, versions and numbers
/// are left by `plain_value`.
fn is_secret_key(key: &str) -> bool {
    let words = key_words(key);
    (has_secret_word(&words) || ends_in_auth(&words))
        && !words
            .last()
            .is_some_and(|w| SECRET_META_WORDS.contains(&w.as_str()))
}

fn has_secret_word(words: &[String]) -> bool {
    words.iter().any(|w| SECRET_KEY_WORDS.contains(&w.as_str()))
        || words
            .windows(2)
            .any(|p| SECRET_KEY_PAIRS.contains(&(p[0].as_str(), p[1].as_str())))
}

fn ends_in_auth(words: &[String]) -> bool {
    words
        .last()
        .is_some_and(|w| SECRET_KEY_LAST_WORDS.contains(&w.as_str()))
}

/// Is the value one to keep, because the key is a secret's name only by its
/// last word `auth` and the value is not a credential: fewer than 8 base64
/// characters, or made of words (`required`, `jwt_bearer`, `authReducer`,
/// `SCRAM-SHA-256`: see `is_word_shaped`)? A base64 `user:pass` has none.
fn auth_value_is_code(key: &str, value: &str) -> bool {
    let words = key_words(key);
    if has_secret_word(&words) || !ends_in_auth(&words) {
        return false;
    }
    // A credential is 8 to 1024 characters: the rest of a line is not scanned
    // again for each assignment of a long line (as for `plain_value`).
    let base64 = (8..=1024).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c));
    !base64 || is_word_shaped(value)
}

/// Does the text read as code or words: only runs of lower-case letters
/// (2 or more), capitalised words (`Reducer`), upper-case acronyms (2 or
/// more, `SHA`), digits, and `_` or `-`? Random base64 breaks the shape with
/// a lone letter, a `+`, `/` or `=`.
///
/// Accepted by TASK-009: about 5-6 % of random 12-character values read as
/// words and stay (0.3 % of base64 `user:pass`); a name with a lone letter
/// next to a digit (`awsSigV4`, `x509_cert`, `k8s-staging`) does not.
fn is_word_shaped(text: &str) -> bool {
    let b = text.as_bytes();
    let run = |from: usize, f: fn(&u8) -> bool| b[from..].iter().take_while(|c| f(c)).count();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_lowercase() {
            if run(i, u8::is_ascii_lowercase) < 2 {
                return false;
            }
            i += run(i, u8::is_ascii_lowercase);
        } else if c.is_ascii_uppercase() {
            let upper = run(i, u8::is_ascii_uppercase);
            let lower = run(i + upper, u8::is_ascii_lowercase);
            if lower > 0 {
                // `Reducer`, or `HTTPBasic`: the last capital opens a word.
                if lower < 2 {
                    return false;
                }
                i += upper + lower;
            } else if upper >= 2 {
                i += upper;
            } else {
                return false;
            }
        } else if c.is_ascii_digit() {
            i += run(i, u8::is_ascii_digit);
        } else if c == b'_' || c == b'-' {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

pub fn redact(stdout: &str, stderr: &str) -> Redaction {
    let mut fired = Vec::new();
    let mut out = stdout.to_string();
    let mut err = stderr.to_string();

    for rule in rules() {
        let placeholder = format!("«redacted:{}»", rule.name);
        let (after_out, n_out) = replace_all_counted(rule, &out, &placeholder);
        let (after_err, n_err) = replace_all_counted(rule, &err, &placeholder);
        if n_out > 0 || n_err > 0 {
            fired.push(rule.name.to_string());
        }
        out = after_out;
        err = after_err;
    }

    // Stable order, no duplicates.
    fired.sort();
    fired.dedup();

    Redaction {
        stdout: out,
        stderr: err,
        rules_fired: fired,
    }
}

fn replace_all_counted(rule: &Rule, text: &str, placeholder: &str) -> (String, usize) {
    let mut count = 0usize;
    if let Mode::Assignment = rule.mode {
        let out = replace_assignments(&rule.re, text, placeholder, &mut count);
        return (out, count);
    }
    let out = rule.re.replace_all(text, |caps: &regex::Captures| {
        let kept = |i: usize| caps.get(i).map_or("", |m| m.as_str());
        let value = match rule.mode {
            Mode::Whole => {
                count += 1;
                return placeholder.to_string();
            }
            Mode::Value => {
                if rule.accept.is_some_and(|accept| !accept(kept(2))) {
                    return caps[0].to_string();
                }
                placeholder.to_string()
            }
            Mode::Assignment => unreachable!("handled by replace_assignments"),
            Mode::KeyValue => {
                let key = caps
                    .name("qk")
                    .or(caps.name("bk"))
                    .map_or("", |m| m.as_str());
                match key_value_replacement(key, &caps["value"], placeholder) {
                    Some(v) => {
                        count += 1;
                        return format!("{}{}", &caps["keep"], v);
                    }
                    None => return caps[0].to_string(),
                }
            }
        };
        count += 1;
        format!("{}{}{}", kept(1), value, kept(3))
    });
    (out.into_owned(), count)
}

/// Replaces the values of secret assignments. The value runs to the end of
/// the line, so when an assignment is not a secret (`FOO=1 API_TOKEN=x cmd`)
/// the search goes on inside its value, from the character before it.
fn replace_assignments(re: &Regex, text: &str, placeholder: &str, count: &mut usize) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut copied, mut pos, mut line_end, mut last_paren) = (0, 0, 0, None);
    while let Some(caps) = re.captures_at(text, pos) {
        let (key, keep) = (&caps["key"], &caps["keep"]);
        let vstart = caps.name("keep").map_or(0, |m| m.end());
        // The value is searched again from the `=` or the blank before it.
        pos = vstart - 1;
        if !is_secret_key(key) && !is_glued_secret_key(key) {
            continue;
        }
        if vstart >= line_end {
            line_end = text[vstart..].find('\n').map_or(text.len(), |i| vstart + i);
            last_paren = text[vstart..line_end].rfind('(').map(|i| vstart + i);
        }
        let has_paren = last_paren.is_some_and(|p| p >= vstart);
        let eq = keep.rfind('=').unwrap_or(0);
        let spaced = eq + 1 < keep.len() || keep[..eq].ends_with([' ', '\t']);
        if let Some(v) =
            assignment_replacement(key, &text[vstart..line_end], placeholder, spaced, has_paren)
        {
            *count += 1;
            out.push_str(&text[copied..vstart]);
            out.push_str(&v);
            (copied, pos) = (line_end, line_end);
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The replacement for the value of a `key = value` assignment, `None` to
/// keep it: a key that is not a secret's name, a value that is no secret
/// (see `plain_value`), or, unquoted, an expression of code: a call
/// (`getToken(req)`), a statement (`... ;`), a member access
/// (`req.headers.authorization`, only with a blank next to the `=`, as code
/// has it: `token=a.b` is a value). A quoted value stays quoted, and what
/// follows the closing quote stays (`"abc";`).
fn assignment_replacement(
    key: &str,
    value: &str,
    placeholder: &str,
    spaced: bool,
    has_paren: bool,
) -> Option<String> {
    if let Some(q) = value.chars().next().filter(|c| *c == '"' || *c == '\'') {
        return match quoted_end(value, q) {
            // Quoted and closed: the quotes and what follows stay, if what
            // follows is clearly not part of the value.
            Some(end)
                if value[end..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace() || ";,)".contains(c)) =>
            {
                let inner = &value[1..end - 1];
                (!plain_value(inner) && !auth_value_is_code(key, inner))
                    .then(|| format!("{q}{placeholder}{q}{}", &value[end..]))
            }
            // `"abc"def`, `'a'\''b'`, no closing quote: all of the value.
            _ => Some(placeholder.to_string()),
        };
    }
    // A member chain without digits (`req.headers.cookie`, `process.env.X`).
    static CODE: OnceLock<Regex> = OnceLock::new();
    let code = CODE.get_or_init(|| {
        Regex::new(r"^[A-Za-z_$][A-Za-z_$]*(?:\.[A-Za-z_$][A-Za-z_$]*)+(?:[^A-Za-z0-9_$.]|$)")
            .expect("code value regex compiles")
    });
    // The value is the rest of the line: a long one is no type, version or
    // variable, and is not scanned again for each assignment of a long line.
    if (value.len() <= 1024 && plain_value(value))
        || has_paren
        || value.trim_end().ends_with(';')
        || (spaced && code.is_match(value))
        || auth_value_is_code(key, value.trim_end())
    {
        return None;
    }
    Some(placeholder.to_string())
}

/// The end (after the closing quote) of the quoted value that begins at
/// `value[0]`, a quote `q`: a double-quoted one has `\` escapes, a
/// single-quoted one has none, as in `key_value_secret`.
fn quoted_end(value: &str, q: char) -> Option<usize> {
    let mut chars = value.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        if c == '\\' && q == '"' {
            chars.next();
        } else if c == q {
            return Some(i + 1);
        }
    }
    None
}

/// A key where a secret word is glued to another word (`PGPASSWORD`,
/// `GITHUBTOKEN`, `-Dpassword`, `SECRETKEY`): such a key is a secret's name
/// in an assignment, unless it ends in a word about the secret.
fn is_glued_secret_key(key: &str) -> bool {
    let words = key_words(key);
    !words
        .last()
        .is_some_and(|w| SECRET_META_WORDS.contains(&w.as_str()))
        && words.iter().any(|w| {
            SECRET_KEY_WORDS
                .iter()
                .chain(&["secretkey"])
                .any(|s| w.ends_with(s))
        })
}

/// The replacement for the value of a `key: value` pair, `None` to keep it:
/// a key that is not a secret's name, an empty string, `null`, `~`, a
/// boolean, a number, a type (`string`, `Optional[str]`), a version
/// (`^9.0.0`, `workspace:*`), a variable (`${API_KEY}`), or a value already
/// redacted.
fn key_value_replacement(key: &str, value: &str, placeholder: &str) -> Option<String> {
    if !is_secret_key(key) {
        return None;
    }
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'');
    let inner = match quote {
        Some(_) => &value[1..value.len() - 1],
        None => value,
    };
    if plain_value(inner) || auth_value_is_code(key, inner) {
        return None;
    }
    Some(match quote {
        Some(q) => format!("{q}{placeholder}{q}"),
        None => placeholder.to_string(),
    })
}

/// Is the text exactly one marker of this module, `«redacted:<rule_name>»`?
fn is_marker(text: &str) -> bool {
    text.strip_prefix("«redacted:")
        .and_then(|rest| rest.strip_suffix('»'))
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// A value that is no secret: see `key_value_replacement`.
fn plain_value(value: &str) -> bool {
    static PLAIN: OnceLock<Regex> = OnceLock::new();
    let re = PLAIN.get_or_init(|| {
        Regex::new(
            r"(?ix)^(?:
                |null|nil|none|undefined|~|true|false|yes|no
                |string|str|number|int|integer|float|bool|boolean|bytes|buffer|any|unknown|object|void
                |secretstr|secretbytes
                |(?:optional|list|dict|set|tuple|union|sequence|mapping|iterable|callable|literal|annotated|type|array|record|promise|map)[\[<].*  # Optional[str], Array<T>
                |[\^~]?(?:[<>]=?|=)?\s*v?\d+(?:\.(?:\d+|x|\*)){1,3}(?:[-+][0-9a-z.\-]+)?   # versions
                |(?:workspace|npm|file|link|git\+\S*|github):.*
                |\$\{[a-z_][a-z0-9_]*(?:[:?\-=+][^}]*)?\}|\$[a-z_][a-z0-9_]*  # ${VAR}, $VAR
                |\$?\{\{.*\}\}                                        # ${{ x }}, {{ x }}
                |![a-z][a-z0-9_!/.\-]*                                  # a YAML tag: !vault
                |«redacted:[a-z0-9_]+»
            )$",
        )
        .expect("plain value regex compiles")
    });
    re.is_match(value)
        || (value.parse::<f64>().is_ok() && value.chars().any(|c| c.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// Output truncation per `send_output` (security.md §8).
// ---------------------------------------------------------------------------

use cgpt_bridge_protocol::agent::SendOutput;

pub struct TruncateOutcome {
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub effective_mode: SendOutput,
}

/// Caps on the post-redaction text we ship back to the assistant.
const SUMMARY_KEEP_HEAD: usize = 2 * 1024;
const TRUNC_KEEP_HEAD: usize = 16 * 1024;
const TRUNC_KEEP_TAIL: usize = 4 * 1024;
const FULL_TOTAL_HARD_CAP: usize = 256 * 1024;

pub fn truncate(stdout: &str, stderr: &str, mode: SendOutput) -> TruncateOutcome {
    match mode {
        SendOutput::Summary => {
            let (o, ot) = head_only(stdout, SUMMARY_KEEP_HEAD);
            let (e, et) = head_only(stderr, SUMMARY_KEEP_HEAD);
            TruncateOutcome {
                stdout: o,
                stderr: e,
                truncated: ot || et,
                effective_mode: SendOutput::Summary,
            }
        }
        SendOutput::Truncated => {
            let (o, ot) = head_tail(stdout, TRUNC_KEEP_HEAD, TRUNC_KEEP_TAIL);
            let (e, et) = head_tail(stderr, TRUNC_KEEP_HEAD, TRUNC_KEEP_TAIL);
            TruncateOutcome {
                stdout: o,
                stderr: e,
                truncated: ot || et,
                effective_mode: SendOutput::Truncated,
            }
        }
        SendOutput::Full => {
            let total = stdout.len() + stderr.len();
            if total <= FULL_TOTAL_HARD_CAP {
                TruncateOutcome {
                    stdout: stdout.to_string(),
                    stderr: stderr.to_string(),
                    truncated: false,
                    effective_mode: SendOutput::Full,
                }
            } else {
                // Downgrade to truncated.
                let (o, _) = head_tail(stdout, TRUNC_KEEP_HEAD, TRUNC_KEEP_TAIL);
                let (e, _) = head_tail(stderr, TRUNC_KEEP_HEAD, TRUNC_KEEP_TAIL);
                TruncateOutcome {
                    stdout: o,
                    stderr: e,
                    truncated: true,
                    effective_mode: SendOutput::Truncated,
                }
            }
        }
    }
}

fn head_only(s: &str, keep: usize) -> (String, bool) {
    if s.len() <= keep {
        return (s.to_string(), false);
    }
    let cut = safe_char_boundary(s, keep);
    let mut out = String::with_capacity(cut + 64);
    out.push_str(&s[..cut]);
    out.push_str(&format!("\n«…{} bytes elided…»\n", s.len() - cut));
    (out, true)
}

fn head_tail(s: &str, head: usize, tail: usize) -> (String, bool) {
    if s.len() <= head + tail {
        return (s.to_string(), false);
    }
    let head_cut = safe_char_boundary(s, head);
    let tail_start = safe_char_boundary_from_end(s, tail);
    let elided = s.len() - head_cut - (s.len() - tail_start);
    let mut out = String::with_capacity(head_cut + (s.len() - tail_start) + 64);
    out.push_str(&s[..head_cut]);
    out.push_str(&format!("\n«…{} bytes elided…»\n", elided));
    out.push_str(&s[tail_start..]);
    (out, true)
}

fn safe_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn safe_char_boundary_from_end(s: &str, from_end: usize) -> usize {
    let start = s.len().saturating_sub(from_end);
    let mut i = start;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_openai_key_in_stdout() {
        let r = redact("sk-abcdefghijklmnopqrstuvwxyz", "");
        assert!(r.stdout.contains("«redacted:openai_api_key»"));
        assert!(r.rules_fired.iter().any(|n| n == "openai_api_key"));
    }

    #[test]
    fn sk_inside_a_word_is_not_a_key() {
        for text in [
            "/Users/me/dev/desk-booking-service-frontend/src/app.ts",
            "@acme/task-scheduler-plugin-config",
            "risk-assessment-pipeline-config.yaml",
            "mask-0123456789abcdefghijklmnop",
            // A URL-encoded path: `%2F` is no separator for a key.
            "https://h/p?dir=%2Fdesk-booking-service-frontend",
            "https://h/p?dir=%2Fsk-payments-gateway-service-frontend",
        ] {
            let r = redact(text, text);
            assert_eq!(r.stdout, text);
            assert_eq!(r.stderr, text);
            assert!(r.rules_fired.is_empty(), "{}: {:?}", text, r.rules_fired);
        }
    }

    #[test]
    fn standalone_sk_keys_are_redacted_with_their_left_neighbour_kept() {
        let key = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        let proj = "sk-proj-abcdefghijklmnopqrstuvwxyz_0123-456789";
        let ant = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";
        for (text, want) in [
            (key.to_string(), "«redacted:openai_api_key»".to_string()),
            (proj.to_string(), "«redacted:openai_api_key»".to_string()),
            (ant.to_string(), "«redacted:anthropic_api_key»".to_string()),
            (
                format!("key is {key} here"),
                "key is «redacted:openai_api_key» here".to_string(),
            ),
            (
                format!("Authorization: Bearer {proj}"),
                "Authorization: Bearer «redacted:openai_api_key»".to_string(),
            ),
            (
                format!("\"apiKey\": \"{key}\""),
                "\"apiKey\": \"«redacted:openai_api_key»\"".to_string(),
            ),
            (
                format!("'{key}'"),
                "'«redacted:openai_api_key»'".to_string(),
            ),
            (
                format!("OPENAI_KEY={key}"),
                "OPENAI_KEY=«redacted:openai_api_key»".to_string(),
            ),
            (
                format!("openai: {key}"),
                "openai: «redacted:openai_api_key»".to_string(),
            ),
            (
                format!("line one\n{key}\n"),
                "line one\n«redacted:openai_api_key»\n".to_string(),
            ),
            // JSON-escaped line breaks and tabs: a literal backslash and a letter.
            (
                format!(r"line\n{key}"),
                r"line\n«redacted:openai_api_key»".to_string(),
            ),
            (
                format!(r"a\r\n{key}\t{proj}"),
                r"a\r\n«redacted:openai_api_key»\t«redacted:openai_api_key»".to_string(),
            ),
            // URL-encoded space, quotes, colon and equals.
            (
                format!("Authorization=Bearer%20{key}"),
                "Authorization=Bearer%20«redacted:openai_api_key»".to_string(),
            ),
            (
                format!("key%3D{ant}%26x"),
                "key%3D«redacted:anthropic_api_key»%26x".to_string(),
            ),
            (
                format!("k%3a{key}%22{key}%27{key}"),
                "k%3a«redacted:openai_api_key»%22«redacted:openai_api_key»%27«redacted:openai_api_key»"
                    .to_string(),
            ),
            (
                format!("{key} {proj}"),
                "«redacted:openai_api_key» «redacted:openai_api_key»".to_string(),
            ),
        ] {
            let r = redact(&text, "");
            assert_eq!(r.stdout, want, "input: {}", text);
            assert!(!r.stdout.contains("abcdefghijkl"), "{}", r.stdout);
        }
    }

    #[test]
    fn redacts_aws_key() {
        let r = redact("AKIAIOSFODNN7EXAMPLE", "");
        assert!(r.stdout.contains("«redacted:aws_access_key_id»"));
    }

    #[test]
    fn redacts_jwt() {
        let r = redact("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIx.SOMEsig_dash-here", "");
        assert!(r.stdout.contains("«redacted:jwt_like»"));
    }

    #[test]
    fn redacts_bearer_in_stderr() {
        let r = redact("", "Authorization: Bearer abc.def-xyz");
        assert!(r.stderr.contains("«redacted:generic_bearer_token»"));
    }

    #[test]
    fn redacts_dotenv_preserving_key() {
        let r = redact("DATABASE_PASSWORD=hunter2-very-real\nOK\n", "");
        assert!(r
            .stdout
            .contains("DATABASE_PASSWORD=«redacted:dotenv_secret_assignment»"));
        assert!(r.stdout.contains("OK"));
    }

    #[test]
    fn redacts_private_key_block_whole() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIBcontentline1\ncontentline2\n-----END PRIVATE KEY-----";
        let r = redact(pem, "");
        assert!(r.stdout.contains("«redacted:private_key_block»"));
        assert!(!r.stdout.contains("MIIBcontent"));
    }

    #[test]
    fn redacts_json_and_yaml_secrets_keeping_keys() {
        let json =
            r#"{"token": "abc123def", "apiKey":"k-999", "user": "bob", "client_secret" : 'xyz'}"#;
        let r = redact(json, "");
        assert!(!r.stdout.contains("abc123def"), "{}", r.stdout);
        assert!(!r.stdout.contains("k-999"), "{}", r.stdout);
        assert!(!r.stdout.contains("xyz"), "{}", r.stdout);
        assert!(
            r.stdout
                .contains(r#""token": "«redacted:key_value_secret»""#),
            "{}",
            r.stdout
        );
        assert!(r.stdout.contains(r#""user": "bob""#));
        assert!(r.rules_fired.iter().any(|n| n == "key_value_secret"));

        let yaml = "db:\n  password: hunter2\n  token: t0k3n\n  host: db.local\n";
        let r = redact(yaml, "");
        assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
        assert!(!r.stdout.contains("t0k3n"), "{}", r.stdout);
        assert!(
            r.stdout
                .contains("  password: «redacted:key_value_secret»\n"),
            "{}",
            r.stdout
        );
        assert!(r.stdout.contains("host: db.local"));
    }

    #[test]
    fn key_value_secret_leaves_empty_null_boolean_and_number_values() {
        for text in [
            r#"{"token": null, "password": "", "api_key": true, "secret": false}"#,
            r#"{"token":null,"x":1}"#,
            "tokens: 4096\nmax_token: 12.5\npassword: ~\npassword:\n",
            r#"{"password": {"min_length": 8}}"#,
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
            assert!(r.rules_fired.is_empty(), "{:?} for {text}", r.rules_fired);
        }
    }

    #[test]
    fn key_value_secret_leaves_code_and_package_names() {
        for text in [
            r#"  "dependencies": { "jsonwebtoken": "^9.0.0", "csrf-token": "1.0.0" },"#,
            r#""password-validator": "^5.3.0", "secret-handshake": "1.1.0","#,
            r#""@octokit/auth-token": "^4.0.0", "x-token": "workspace:*", "y-token": "npm:z@1""#,
            r#"{ "secretName": "prod-db", "tokenTtl": 3600, "passwordMinLength": 8 }"#,
            "interface Opts { token: string; secret?: Buffer }",
            "def f(token: str, password: Optional[str] = None):",
            "    secret: str = load_secret(name=\"db\")",
            "token_count: \"12\"",
            r#"log.info("token: %s", token[:4])"#,
            "foo(secret, token: string)",
            "Invalid token: signature mismatch",
            "      API_KEY: ${API_KEY}\n      TOKEN: $TOKEN\n",
            r#"{"password": "${DB_PASSWORD}"}"#,
            "    # password: must be at least 8 chars",
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
            assert!(r.rules_fired.is_empty(), "{:?} for {text}", r.rules_fired);
        }
    }

    #[test]
    fn key_value_secret_keeps_redacting_config_secrets() {
        for (text, secret) in [
            ("      DB_PASSWORD: \"s3cret\"\n", "s3cret"),
            ("      JWT_SECRET: abcdefgh\n", "abcdefgh"),
            (
                "  user:\n    token: kubeTOKENabc123def456\n",
                "kubeTOKENabc123def456",
            ),
            ("    password: p@ssw0rd!\n", "p@ssw0rd!"),
            ("- password: hunter2\n", "hunter2"),
            (r#""apiKey": "3f9a8b7c6d5e""#, "3f9a8b7c6d5e"),
            (r#"{ "X-Api-Token": "abc.def-ghi" }"#, "abc.def-ghi"),
            (r#"{"auth_token":"tok123"}"#, "tok123"),
            (r#"{"password": "Ab3[x9"}"#, "Ab3[x9"),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(secret), "{}", r.stdout);
            assert!(
                r.rules_fired.iter().any(|n| n == "key_value_secret"),
                "{text}"
            );
        }
    }

    #[test]
    fn key_value_secret_finds_the_secret_word_in_any_part_of_the_key() {
        for key in [
            "secret_key_base",
            "SECRET_KEY_BASE",
            "secret_access_key",
            "aws_secret_access_key",
            "secretAccessKey",
            "secret_key",
            "secretKey",
            "db_password_prod",
            "password_prod",
            "password1",
            "tokenValue",
            "apiKeyValue",
            "x-api-key",
            "client_secret",
        ] {
            let json = format!(r#"{{"{key}": "v4lue-0123456789"}}"#);
            let r = redact(&json, "");
            assert!(
                !r.stdout.contains("v4lue-0123456789"),
                "{key}: {}",
                r.stdout
            );
            let yaml = format!("  {key}: v4lue-0123456789\n");
            let r = redact(&yaml, "");
            assert!(
                !r.stdout.contains("v4lue-0123456789"),
                "{key}: {}",
                r.stdout
            );
        }
    }

    #[test]
    fn key_value_secret_leaves_templates_tags_and_secret_types() {
        for text in [
            "          token: ${{ secrets.GITHUB_TOKEN }}\n",
            "          password: ${{ secrets.REGISTRY_PASSWORD }}\n",
            "password: \"{{ vault_password }}\"\n",
            "password: {{ vault_password }}\n",
            "token: !vault |\n",
            "    api_key: SecretStr\n",
            r#"{"secretName": "prod-db", "password_file": "/run/secrets/db"}"#,
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
            assert!(r.rules_fired.is_empty(), "{:?} for {text}", r.rules_fired);
        }
    }

    #[test]
    fn redacts_json_auth_headers_cookies_npmrc_and_kube_keys() {
        let r = redact(
            r#"{ "Authorization": "Bearer 3f9a8b7c6d5e4f3a2b1c", "Cookie": "session=abc123; theme=dark" }"#,
            "",
        );
        assert!(!r.stdout.contains("3f9a8b7c6d5e4f3a2b1c"), "{}", r.stdout);
        assert!(
            r.stdout
                .contains(r#""Authorization": "Bearer «redacted:generic_bearer_token»""#),
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains("session=abc123"), "{}", r.stdout);
        let r = redact(
            "Cookie: sid=s3cr3t\nSet-Cookie: sid=s3cr3t2; HttpOnly\n",
            "",
        );
        assert!(!r.stdout.contains("s3cr3t"), "{}", r.stdout);

        let r = redact("//nexus.corp/repository/npm/:_auth=dXNlcjpwYXNz\n_auth = dXNlcjpwYXNz\nalways-auth=true\n", "");
        assert!(!r.stdout.contains("dXNlcjpwYXNz"), "{}", r.stdout);
        assert!(r.stdout.contains("always-auth=true"));

        let kube = "    client-key-data: LS0tLS1CRUdJTiBSU0EgUFJJVkFURSBLRVktLS0tLQpNSUlFb3dJQkFBS0NBUUVB\n";
        let r = redact(kube, "");
        assert!(
            !r.stdout.contains("LS0tLS1CRUdJTiBSU0EgUFJJVkFURSBLRVkt"),
            "{}",
            r.stdout
        );
        let other = "key.pem: LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0tCk1JSUV2Z0lCQURBTkJna3Foa2lH\n";
        let r = redact(other, "");
        assert!(
            !r.stdout.contains("LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0t"),
            "{}",
            r.stdout
        );
        // A base64 certificate is not a secret.
        let cert = "    certificate-authority-data: LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCk1JSUM=\n";
        assert_eq!(redact(cert, "").stdout, cert);
    }

    #[test]
    fn netrc_password_leaves_prose() {
        for text in [
            "password reset link sent to your email\n",
            "Password must be at least 8 characters\n",
            "log in with your login and password as usual\n",
        ] {
            assert_eq!(redact(text, "").stdout, text);
        }
    }

    #[test]
    fn redacts_url_userinfo_password() {
        let r = redact("cloning https://alice:s3cr3t@git.example.com/r.git", "");
        assert_eq!(
            r.stdout,
            "cloning https://alice:«redacted:url_userinfo_password»@git.example.com/r.git"
        );
        let r = redact("postgres://app:p%40ss@db:5432/main", "");
        assert!(!r.stdout.contains("p%40ss"), "{}", r.stdout);
        let r = redact(
            "see https://example.com:8443/path and git@github.com:o/r",
            "",
        );
        assert!(r.rules_fired.is_empty(), "{:?}", r.rules_fired);
    }

    #[test]
    fn redacts_encrypted_private_key_block() {
        let pem = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIFsecret\n-----END ENCRYPTED PRIVATE KEY-----";
        let r = redact(pem, "");
        assert!(!r.stdout.contains("MIIFsecret"), "{}", r.stdout);
        assert!(r.rules_fired.iter().any(|n| n == "private_key_block"));
    }

    #[test]
    fn redacts_netrc_password() {
        let r = redact("machine api.example.com login bob password pa55w0rd\n", "");
        assert!(!r.stdout.contains("pa55w0rd"), "{}", r.stdout);
        let r = redact("machine x\n  login bob\n  password pa55w0rd\n", "");
        assert!(!r.stdout.contains("pa55w0rd"), "{}", r.stdout);
        assert!(r.stdout.contains("login bob"));
    }

    const CODE_LINES: &[&str] = &[
        "max_tokens = 1024",
        "max_tokens=4096",
        "const token = req.headers.authorization;",
        "const apiKey = process.env.API_KEY || \"\";",
        "PASSWORD_MIN_LENGTH = 8",
        "let cookie = parse(header);",
        "const token = getToken(req);",
        "token = os.environ[\"TOKEN\"]",
        "this.password = form.password",
        "password_hash = bcrypt.hash(pw)",
        "secret = load_secret(name=\"db\")",
        "tokenizer = AutoTokenizer.from_pretrained(name)",
        "if token == expected {",
        "if (token != null && secret != other) {",
        "const f = token => token.trim()",
        "TOKEN=$(pass show api/token)",
        "API_KEY=${API_KEY}",
        "    export SECRET=$SECRET",
        "const secretName = \"prod-db\"",
        "token = None",
        "cookie = parse(header)",
        "document.cookie = \"a=b\"",
        "const cookie = req.headers.cookie;",
        "res.setHeader(\"Set-Cookie\", value);",
        "const cookieName = \"sid\";",
        "if (cookie) { return cookie; }",
        "  cookie: string;",
        "  cookie?: string;",
        "cookie: str = Field(default=None)",
        "cookie: req.headers.cookie,",
        "cookie: cookies.join('; '),",
        "MAX_TOKENS = 1024",
        "const jsonwebtoken = require(\"jsonwebtoken\");",
        "const token = req.headers.cookie;",
        "const secret = this.config.token;",
        "const key = process.env.API_KEY || \"\";",
        "document.cookie=a+\"=\"+b",
        "x.cookie = y.cookie",
        "refreshToken = tokens.refresh_token",
        "password = config.db.password",
        "const token = req.headers.authorization",
    ];

    #[test]
    fn code_lines_that_look_like_secret_assignments_stay() {
        for text in CODE_LINES {
            let r = redact(text, "");
            assert_eq!(r.stdout, *text);
            assert!(r.rules_fired.is_empty(), "{:?} for {text}", r.rules_fired);
        }
        let all = CODE_LINES.join("\n");
        assert_eq!(redact(&all, "").stdout, all);
    }

    #[test]
    fn dotenv_assignment_keeps_redacting_real_secrets() {
        for (text, secret, kept) in [
            ("API_TOKEN=abc123xyz789\n", "abc123xyz789", "API_TOKEN="),
            (
                "DB_PASSWORD=\"hunter2-very\"\n",
                "hunter2-very",
                "DB_PASSWORD=",
            ),
            (
                "SECRET_KEY=django-insecure-k3y\n",
                "django-insecure-k3y",
                "SECRET_KEY=",
            ),
            (
                "export TOKEN=\"abc def ghi\"\n",
                "abc def ghi",
                "export TOKEN=",
            ),
            (
                "export API_KEY='k3y-value'\n",
                "k3y-value",
                "export API_KEY=",
            ),
            (
                "  PRIVATE_KEY = s3cr3t-value\n",
                "s3cr3t-value",
                "PRIVATE_KEY = ",
            ),
            ("db.password=s3cret-value\n", "s3cret-value", "db.password="),
            (
                "mysql --user=root --password=hunter2x\n",
                "hunter2x",
                "--password=",
            ),
            (
                "docker run -e API_TOKEN=abc123xyz789 img\n",
                "abc123xyz789",
                "-e API_TOKEN=",
            ),
            (
                "FOO=1 API_TOKEN=abc123xyz789 cmd\n",
                "abc123xyz789",
                "FOO=1 API_TOKEN=",
            ),
            (
                "const token = \"abc123xyz789\";\n",
                "abc123xyz789",
                "const token = ",
            ),
            ("password = hunter2-very\n", "hunter2-very", "password = "),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(secret), "{text} -> {}", r.stdout);
            assert!(r.stdout.contains(kept), "{text} -> {}", r.stdout);
            assert!(r.stdout.contains("«redacted:dotenv_secret_assignment»"));
            assert!(
                r.rules_fired
                    .iter()
                    .any(|n| n == "dotenv_secret_assignment"),
                "{text}"
            );
        }
        // The quotes and what follows the value stay.
        assert_eq!(
            redact("const token = \"abc123xyz789\";", "").stdout,
            "const token = \"«redacted:dotenv_secret_assignment»\";"
        );
    }

    #[test]
    fn cookie_header_only_in_the_form_of_a_header() {
        for text in [
            "Cookie: session=abc123def\n",
            "  cookie: sid=s3cr3tvalue\n",
            "Set-Cookie: sid=s3cr3tvalue; HttpOnly\n",
            "> Cookie: session=abc123def\n",
            "curl -H 'Cookie: session=abc123def' https://x.test\n",
            r#"curl -H "Cookie: session=abc123def" https://x.test"#,
            r#"{ "Cookie": "session=abc123def; theme=dark" }"#,
            r#"{ 'cookie': 'session=abc123def' }"#,
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains("abc123def"), "{text} -> {}", r.stdout);
            assert!(!r.stdout.contains("s3cr3tvalue"), "{text} -> {}", r.stdout);
            assert!(
                r.rules_fired.iter().any(|n| n == "cookie_header"),
                "{text} -> {}",
                r.stdout
            );
        }
    }

    #[test]
    fn glued_secret_keys_are_secret_keys_in_assignments() {
        for (text, secret) in [
            ("PGPASSWORD=fakehunter2 psql -h db\n", "fakehunter2"),
            ("GITHUBTOKEN=fakeabc123xyz\n", "fakeabc123xyz"),
            ("ADMINPASSWORD=fakeabc123xyz\n", "fakeabc123xyz"),
            ("SECRETKEY=fakeabc123xyz\n", "fakeabc123xyz"),
            ("java -Dpassword=fakehunter2 -jar app.jar\n", "fakehunter2"),
            ("mvn test -Dtoken=fakeabc123xyz\n", "fakeabc123xyz"),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(secret), "{text} -> {}", r.stdout);
            assert!(
                r.rules_fired
                    .iter()
                    .any(|n| n == "dotenv_secret_assignment"),
                "{text}"
            );
        }
    }

    #[test]
    fn glued_keys_do_not_make_code_keys_secret() {
        for text in [
            "PASSWORD_MIN_LENGTH = 8",
            "MAX_TOKENS = 1024",
            "max_tokens = 1024",
            "PASSWORD_FILE=/run/secrets/db",
            "const jsonwebtoken = require(\"jsonwebtoken\");",
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
            assert!(r.rules_fired.is_empty(), "{:?} for {text}", r.rules_fired);
        }
    }

    #[test]
    fn quoted_assignment_values_leak_nothing_after_the_first_quote() {
        for (text, tail) in [
            (r#"DB_PASSWORD="p4ss\"w0rdFAKEtail""#, "FAKEtail"),
            (r#"PASSWORD='fake'\''s3cretFAKEtail'"#, "FAKEtail"),
            (r#"API_TOKEN="fakeabc"FAKEtail123"#, "FAKEtail"),
            (r#"const token = "fake\"FAKEtail123";"#, "FAKEtail"),
            (r#"API_TOKEN="fakeabcFAKEtail123"#, "FAKEtail"),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(tail), "{text} -> {}", r.stdout);
            assert!(!r.stdout.contains("w0rd"), "{text} -> {}", r.stdout);
            assert!(!r.stdout.contains("s3cret"), "{text} -> {}", r.stdout);
            assert!(r
                .rules_fired
                .iter()
                .any(|n| n == "dotenv_secret_assignment"));
        }
        // A well-formed quoted value keeps its quotes and what follows.
        assert_eq!(
            redact(r#"DB_PASSWORD="p4ss\"w0rd" # prod"#, "").stdout,
            r#"DB_PASSWORD="«redacted:dotenv_secret_assignment»" # prod"#
        );
    }

    #[test]
    fn dotted_tokens_are_values_not_member_access() {
        for (text, secret) in [
            (
                "VAULT_TOKEN=hvs.CAESIFAKE0123abcd\n",
                "hvs.CAESIFAKE0123abcd",
            ),
            ("VAULT_TOKEN=s.FAKEfake0123abcd\n", "s.FAKEfake0123abcd"),
            (
                "GOOGLE_ACCESS_TOKEN=ya29.a0AfB_byFAKE\n",
                "ya29.a0AfB_byFAKE",
            ),
            (
                "DISCORD_TOKEN=MTk4NjIyNDgzNDcx.Cl2FMQ.ZnCjFAKE\n",
                "MTk4NjIyNDgzNDcx",
            ),
            (
                "MAPBOX_SECRET_TOKEN=sk.eyJ1IjoiZmFrZSJ9\n",
                "sk.eyJ1IjoiZmFrZSJ9",
            ),
            ("DB_PASSWORD=hunter2.backup\n", "hunter2.backup"),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(secret), "{text} -> {}", r.stdout);
        }
    }

    #[test]
    fn cookie_header_is_found_anywhere_in_a_line() {
        for text in [
            "2026-10-27 12:00:00 DEBUG Cookie: session=fakeabc123\n",
            "curl -H Cookie:session=fakeabc123 https://x.test\n",
            "header: Set-Cookie: sid=fakeabc123; Path=/\n",
            "{ host: 'x.test', cookie: 'session=fakeabc123' }\n",
            "map[Cookie:[session=fakeabc123] Host:[x.test]]\n",
            "fetch(url, { headers: { Cookie: \"session=fakeabc123\" } })\n",
            "Set-Cookie: __Host-sid=fakeabc123; Secure\n",
            "curl -H ^\"cookie: session=fakeabc123^\" https://x.test\n",
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains("fakeabc123"), "{text} -> {}", r.stdout);
            assert!(
                r.rules_fired.iter().any(|n| n == "cookie_header"),
                "{text} -> {}",
                r.stdout
            );
        }
    }

    #[test]
    fn accepted_by_task_007_card_values_that_look_like_code_stay() {
        // accepted by TASK-007 card: a trailing `;`, a `(` and a plain
        // number mark an unquoted value as code or non-secret.
        for text in [
            "export API_TOKEN=fakeabc123;",
            "DB_PASSWORD=P(ssw0rd-fake",
            "TOKEN=123456789012",
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
        }
    }

    #[test]
    fn long_lines_of_assignments_do_not_overflow_the_stack() {
        // A request is handled on a thread with a 2 MiB stack.
        let one_line = "a=1 ".repeat(50_000);
        let minified = "var a=b.c,d=e(f);if(g){h=i.j}".repeat(7_000);
        let secrets = "token = a.b ".repeat(20_000);
        let started = std::time::Instant::now();
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                for text in [&one_line, &minified, &secrets] {
                    let r = redact(text, "");
                    assert_eq!(&r.stdout, text);
                    assert!(r.rules_fired.is_empty(), "{:?}", r.rules_fired);
                }
            })
            .unwrap();
        handle.join().unwrap();
        eprintln!("long lines redacted in {:?}", started.elapsed());
    }

    #[test]
    fn cookie_with_a_glued_equals_sign_is_a_header_again() {
        for text in [
            "Cookie=session=fakeabc123\n",
            "cookie=sid=fakeabc123; theme=dark\n",
            "Set-Cookie=sid=fakeabc123\n",
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains("fakeabc123"), "{text} -> {}", r.stdout);
            assert!(r.rules_fired.iter().any(|n| n == "cookie_header"), "{text}");
        }
    }

    #[test]
    fn dotted_values_with_no_space_around_equals_are_redacted() {
        for (text, secret) in [
            ("SECRET=fakeabc.fakedefFAKE\n", "fakeabc"),
            ("DB_PASSWORD=fake.hunterFAKE\n", "hunterFAKE"),
            ("TOKEN=FAKEabc.FAKEdef.FAKEghi\n", "FAKEdef"),
            ("API_TOKEN=a.b\n", "a.b"),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(secret), "{text} -> {}", r.stdout);
        }
    }

    #[test]
    fn a_quoted_value_followed_by_a_hash_leaks_nothing() {
        let r = redact("API_TOKEN=\"fakeabc\"#FAKEtail123", "");
        assert!(!r.stdout.contains("FAKEtail"), "{}", r.stdout);
        // After a space it is a comment, and stays.
        assert_eq!(
            redact("API_TOKEN=\"fakeabc\" # prod", "").stdout,
            "API_TOKEN=\"«redacted:dotenv_secret_assignment»\" # prod"
        );
    }

    #[test]
    fn a_value_is_plain_only_when_it_is_exactly_one_marker() {
        let r = redact(
            "OPENAI_API_KEY=sk-proj-fakeabcdefghijklmnopqrstuvwx.FAKEtail\n",
            "",
        );
        assert!(!r.stdout.contains("FAKEtail"), "{}", r.stdout);
        // A value redacted by an earlier rule is left to that rule.
        let r = redact("AUTH_TOKEN=eyJfakeabc.fakedef.fakeghi\n", "");
        assert_eq!(r.stdout, "AUTH_TOKEN=«redacted:jwt_like»\n");
        assert!(!r
            .rules_fired
            .iter()
            .any(|n| n == "dotenv_secret_assignment"));
    }

    #[test]
    fn accepted_by_task_007_ruling_11_ambiguous_shapes_stay() {
        // accepted by TASK-007 / ruling 11: after a closing quote a `,` may
        // be an argument separator of code (`f(password="x",host=h)`).
        assert_eq!(
            redact("API_TOKEN=\"fakeabc\",host", "").stdout,
            "API_TOKEN=\"«redacted:dotenv_secret_assignment»\",host"
        );
        // accepted by TASK-007 / ruling 11: real cookie headers carry
        // `name=value` pairs, a bare value is not one.
        for text in ["Cookie: fakeabc123\n", "  cookie: fakeabc123\n"] {
            assert_eq!(redact(text, "").stdout, text);
        }
        // accepted by TASK-007 / ruling 11: not redacted at the base either.
        let curl = "curl -b 'sid=fakeabc123' https://x.test\n";
        assert_eq!(redact(curl, "").stdout, curl);
    }

    #[test]
    fn repeated_kept_assignments_on_one_line_do_not_take_quadratic_time() {
        // A request is handled on a thread with a 2 MiB stack.
        let texts: Vec<String> = ["token = Optional[x ", "token = file:x ", "token = a.b "]
            .iter()
            .map(|unit| unit.repeat(100_000))
            .collect();
        for text in texts {
            let started = std::time::Instant::now();
            let handle = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || redact(&text, "").stdout.len())
                .unwrap();
            handle.join().unwrap();
            let took = started.elapsed();
            eprintln!("repeated assignments redacted in {took:?}");
            // A backstop, far above the milliseconds of a linear pass.
            assert!(took < std::time::Duration::from_secs(10), "{took:?}");
        }
    }

    #[test]
    fn cookie_with_a_quoted_or_spaced_equals_sign_is_a_header_again() {
        for text in [
            "Cookie = sid=fakeabc123; theme=dark\n",
            "cookie=\"sid=fakeabc123\"\n",
            "\"Cookie\"=\"sid=fakeabc123\"\n",
            "\"Cookie\"=sid=fakeabc123\n",
            "Invoke-WebRequest -Headers @{Cookie=\"sid=fakeabc123\"} https://x.test\n",
            "Invoke-WebRequest -Headers @{\"Cookie\"=\"sid=fakeabc123\"} https://x.test\n",
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains("fakeabc123"), "{text} -> {}", r.stdout);
            assert!(r.rules_fired.iter().any(|n| n == "cookie_header"), "{text}");
        }
        // redacted at base too, ruling 13: minified code that assigns a
        // cookie pair is not told from a header.
        assert_eq!(
            redact("document.cookie=\"theme=dark\"", "").stdout,
            "document.cookie=\"«redacted:cookie_header»\""
        );
        // A spaced assignment of a string is code and stays.
        let code = "document.cookie = \"theme=dark\";";
        assert_eq!(redact(code, "").stdout, code);
    }

    #[test]
    fn gitlab_google_and_slack_tokens_are_redacted() {
        let google = format!("AIza{}", "fake_".repeat(7));
        assert_eq!(google.len(), 39);
        let cases: Vec<(String, &str, &str)> = vec![
            (
                "glpat-fake0fakefakefakefake".into(),
                "glpat-",
                "gitlab_token",
            ),
            ("gldt-fake0fakefakefakefake".into(), "gldt-", "gitlab_token"),
            ("glrt-fake0fakefakefakefake".into(), "glrt-", "gitlab_token"),
            (
                "glptt-fake0fakefakefakefake".into(),
                "glptt-",
                "gitlab_token",
            ),
            (
                "glcbt-fake0fakefakefakefake".into(),
                "glcbt-",
                "gitlab_token",
            ),
            // The routable form: `<payload>.<two digits>.<checksum>`.
            (
                "glpat-fake0fakefakefakefake.01.0fakefak".into(),
                "fake0fakefakefakefake",
                "gitlab_token",
            ),
            (google.clone(), "fake_fake_", "google_api_key"),
            (
                "xo\x78c-0000000000-0000000000-0000000000-fakefakefakefake".into(),
                "0000000000",
                "slack_token",
            ),
            (
                "xo\x78b-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            (
                "xo\x78p-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            (
                "xo\x78o-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            (
                "xo\x78a-2-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            (
                "xo\x78s-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            (
                "xo\x78r-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            (
                "xo\x78e-1-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            // A GitLab token with a capital instead of a digit.
            (
                "glpat-FAKEfakefakefakefake".into(),
                "FAKEfake",
                "gitlab_token",
            ),
            (
                "xo\x78e-0000000000-fakefakefakefake".into(),
                "fakefake",
                "slack_token",
            ),
            // The `d` cookie token is URL-encoded.
            (
                "xo\x78d-fakefake%2Bfakefake%2Ffake%3D".into(),
                "fakefake",
                "slack_token",
            ),
        ];
        for (token, fragment, rule) in &cases {
            for text in [
                token.clone(),
                format!("token is {token} here"),
                format!("GITLAB_TOKEN={token}"),
                format!("\"v\": \"{token}\""),
                format!("'{token}'"),
                format!("({token})"),
                format!("url?t={token}&x=1"),
                format!("line one\n{token}\nline three"),
            ] {
                let r = redact(&text, &text);
                assert!(!r.stdout.contains(fragment), "{text} -> {}", r.stdout);
                assert!(!r.stderr.contains(fragment), "{text} -> {}", r.stderr);
                assert!(
                    r.rules_fired
                        .iter()
                        .any(|n| n == rule || n == "dotenv_secret_assignment"),
                    "{text} -> {:?}",
                    r.rules_fired
                );
            }
            // Alone or after a space, the token's own rule fires and the
            // neighbours stay.
            let r = redact(&format!("a {token} b"), "");
            assert_eq!(r.stdout, format!("a «redacted:{rule}» b"));
            assert_eq!(r.rules_fired, vec![rule.to_string()]);
        }
    }

    #[test]
    fn token_prefixes_inside_a_word_or_too_short_are_not_tokens() {
        let google_body = "fake_".repeat(7);
        for text in [
            // not at the left border of a token
            "my-glpat-fake0fakefakefakefake".to_string(),
            "my_gldt-fake0fakefakefakefake".to_string(),
            "xglrt-fake0fakefakefakefake".to_string(),
            "prefix-xo\x78b-0000000000-fakefakefakefake".to_string(),
            "wxo\x78b-0000000000-fakefakefakefake".to_string(),
            format!("xAIza{google_body}"),
            format!("my-AIza{google_body}"),
            // too short
            "glpat-short".to_string(),
            "glpat-".to_string(),
            "xo\x78b-short".to_string(),
            "AIzaShort".to_string(),
            format!("AIza{}", "a".repeat(34)),
            // The glued prefix leaks by design: the left border of TASK-006
            // (as for `sk-`) means `prefix-xoxb-…` is not redacted (above).
            // kebab-case names with a GitLab prefix: a body of letters and
            // hyphens only is no token, a random one has a digit or a capital
            "gldt-service-deployment-tools".to_string(),
            "glimt-bodo-football-club-analytics".to_string(),
            "npm install glimt-dashboard-widgets-core".to_string(),
            "@scope/glimt-ui-components-library".to_string(),
            "/srv/apps/glagent-config-for-production/README".to_string(),
            "https://gitlab.example.com/group/gldt-service-deployment-tools/-/issues/1".to_string(),
            "glrt-runner-registration-helpers-v".to_string(),
            "gloas-oauth-application-secret-docs".to_string(),
            "glft-feed-token-rotation-policy-notes".to_string(),
            // slugs that start like a Slack token: the body of a real one
            // starts with a digit (`xoxd-` is URL-encoded), and `f`-`z` are
            // no Slack types
            "xo\x78o-hugs-and-kisses".to_string(),
            "xo\x78o-gossip-girl-wallpapers-hd".to_string(),
            "class=\"xo\x78o-heart-icon-large\"".to_string(),
            "xo\x78f-0000000000-fakefakefake".to_string(),
            "xo\x78z-0000000000-fakefakefake".to_string(),
            // words, paths and names that only look similar
            "xoxo hugs and kisses".to_string(),
            "glpat is the GitLab token prefix".to_string(),
            "/usr/local/share/glrt-docs/README.md".to_string(),
            "AIzaway is not a word".to_string(),
            "https://gitlab.example.com/group/gldt-service/-/issues/1".to_string(),
        ] {
            let r = redact(&text, &text);
            assert_eq!(r.stdout, text);
            assert_eq!(r.stderr, text);
            assert!(r.rules_fired.is_empty(), "{text}: {:?}", r.rules_fired);
        }
    }

    #[test]
    fn auth_key_with_a_base64_value_is_redacted() {
        // A container registry config: `{"auths": {"host": {"auth": ...}}}`.
        let docker = r#"{"auths": {"registry.example.com": {"auth": "dXNlcjpwYXNz"}}}"#;
        let r = redact(docker, "");
        assert!(!r.stdout.contains("dXNlcjpwYXNz"), "{}", r.stdout);
        assert!(
            r.stdout
                .contains(r#""auth": "«redacted:key_value_secret»""#),
            "{}",
            r.stdout
        );
        assert!(r.stdout.contains(r#""auths""#) && r.stdout.contains("registry.example.com"));
        for text in [
            // letters only, quoted or not: no word shape
            "auth: ZmFrZTpmYWtlcGFzcw\n",
            "auth: \"ZmFrZTpmYWtlcGFzcw\"\n",
            "auth: 'ZmFrZTpmYWtlcGFzcw'\n",
            "AUTH=ZmFrZTpmYWtlcGFzcw\n",
            "auth = ZmFrZTpmYWtlcGFzcw\n",
            "\"auth\": \"ZmFrZTpmYWtlcGFzcw\"",
            "auth: dGVzdDp0ZXN0MQ==\n",
            "    auth: dGVzdDp0ZXN0MQ==\n",
            "auth: \"dXNlcjpwYXNz\"\n",
            "registry_auth: dGVzdDp0ZXN0MQ==\n",
            "\"basicAuth\": \"dXNlcjpwYXNz\"",
            "auth = dGVzdDp0ZXN0MQ==\n",
            "REGISTRY_AUTH=dGVzdDp0ZXN0MQ==\n",
            "\"auth\": \"dGVzdDp0ZXN0MQ==\"",
            "const auth = \"dXNlcjpwYXNz\";",
            "{\"auth\":\"dXNlcjpwYXNz\"}",
        ] {
            let r = redact(text, "");
            assert!(
                !r.stdout.contains("dXNlcjpwYXNz")
                    && !r.stdout.contains("dGVzdDp0ZXN0MQ")
                    && !r.stdout.contains("ZmFrZTpm"),
                "{text} -> {}",
                r.stdout
            );
        }
    }

    #[test]
    fn auth_value_shape_accepted_misses_and_hits() {
        // Accepted by TASK-009 (about 5-6 % of random 12-character values
        // read as words): a letters-only value that reads as words is
        // code, not a credential.
        let word_like = "auth: \"abcdefghijkl\"\n";
        assert_eq!(redact(word_like, "").stdout, word_like);
        // Accepted: a name with a lone letter next to a digit is no word.
        for name in ["awsSigV4", "AzureADB2C", "x509_cert", "k8s-staging"] {
            assert_eq!(
                redact(&format!("auth: {name}\n"), "").stdout,
                "auth: «redacted:key_value_secret»\n",
                "{name}"
            );
        }
    }

    #[test]
    fn oauth_token_is_a_secret_and_oauth_alone_is_not() {
        for text in [
            "oauth_token: fakeoauthvalue123\n",
            "oauthToken = fakeoauthvalue123\n",
            "OAUTH_TOKEN=fakeoauthvalue123\n",
            "\"oauth_token\": \"fakeoauthvalue123\"",
            "OAUTH_CLIENT_SECRET=fakeoauthvalue123\n",
        ] {
            let r = redact(text, "");
            assert!(
                !r.stdout.contains("fakeoauthvalue123"),
                "{text} -> {}",
                r.stdout
            );
        }
        for text in [
            "oauth_callback: /auth/callback\n",
            "OAUTH_CALLBACK=https://app.example.com/cb\n",
            "oauth_provider = github\n",
            "oauth: github\n",
            "oauth = {}\n",
            "OAUTH_REDIRECT_URI=http://localhost:3000/cb\n",
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
            assert!(r.rules_fired.is_empty(), "{text}: {:?}", r.rules_fired);
        }
    }

    #[test]
    fn auth_inside_other_words_is_not_a_secret_key() {
        for text in [
            "author: Jane Doe\n",
            "author = \"Jane Doe\"\n",
            "authority: login.example.com\n",
            "authority = login.example.com\n",
            "authors: a b\n",
            "authenticated = yes\n",
            "authMiddleware: requireLogin\n",
            "const authMiddleware = createMiddleware;",
            "authMiddleware = createMiddleware\n",
            "auth_provider: github\n",
            "auth_url: https://login.example.com/\n",
            "auth_header: Authorization\n",
            "authorization_endpoint: https://login.example.com/\n",
            "auth: none\n",
            "auth = false\n",
            "auth: true\n",
            "auth = require(\"./auth\")\n",
            "const auth = useAuth()",
            "const auth = getAuth(app);",
            "const { auth } = useContext(Ctx);",
            // `auth` is a common name in code: a value that is not a
            // credential stays.
            "self.auth = auth",
            "auth = auth or default\n",
            "  auth: authReducer,",
            "  auth: AuthState;",
            "  auth: Auth\n",
            "auth: basic\n",
            "auth: bearer\n",
            "auth: \"basic\"\n",
            "auth = \"none\"\n",
            "auth = HTTPBasicAuth(user, password)",
            // words of config and code: camelCase, snake_case, kebab-case,
            // SCREAMING_CASE, plain lowercase words
            "\"auth\": \"required\"",
            "\"auth\": \"anonymous\"",
            "\"auth\": \"disabled\"",
            "\"auth\": \"optional\"",
            "\"auth\": \"internal\"",
            "\"auth\": \"keycloak\"",
            "{\"auth\":\"internal\"}",
            "auth: \"firebase\"\n",
            "auth: 'password'\n",
            "auth: \"userpass\"\n",
            "auth: \"bearer_token\"\n",
            "auth: \"OAuth2Client\"\n",
            "auth: jwt_bearer\n",
            "auth: auth_reducer\n",
            "auth: authReducer2\n",
            "auth: auth0_client\n",
            "auth: kebab-case-strategy\n",
            "auth: HmacSHA256\n",
            "auth: AuthenticationMiddleware\n",
            "auth = Auth0Provider\n",
            "auth = auth_service\n",
            "self.auth = auth_backend\n",
            "AUTH=sigv4_auth\n",
            "MONGO_AUTH=SCRAM-SHA-256\n",
            "AUTH_MODE=SCRAM-SHA-256\n",
            "AUTH=LDAP_SIMPLE\n",
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
            assert!(r.rules_fired.is_empty(), "{text}: {:?}", r.rules_fired);
        }
    }

    #[test]
    fn new_token_rules_stay_linear_on_long_lines() {
        let texts: Vec<String> = [
            "glpat-fake ",
            "glpat-fakefakefakefakefak ",
            "AIzafakefake ",
            "xo\x78b-fake ",
            "xo\x78b-fakefakefake- ",
            "auth = a.b ",
            "auth: x ",
            "x_auth=aa_",
            "auth=authReducer ",
            "gldt-aaaaaaaaaaaaaaaaaaaaaaaa ",
            "xo\x78o-aaaaaaaaaaaaaaaa ",
        ]
        .iter()
        .map(|unit| unit.repeat(100_000))
        .collect();
        for text in texts {
            let started = std::time::Instant::now();
            let handle = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || redact(&text, "").stdout.len())
                .unwrap();
            handle.join().unwrap();
            let took = started.elapsed();
            eprintln!("new token rules redacted in {took:?}");
            // A backstop, far above a linear pass (the slowest input takes
            // about 6 s in a debug build, 0.3 s in release) and far below
            // the minutes a quadratic one would need.
            assert!(took < std::time::Duration::from_secs(30), "{took:?}");
        }
    }

    #[test]
    fn chained_auth_assignments_ending_in_a_non_base64_char_stay_linear() {
        // Each kept `auth` value is the rest of the line; its shape is
        // looked at for the first 1 KB only, not once per assignment over
        // the whole line.
        for unit in ["x_auth=", "auth=aa_", "basicAuth=", "AUTH=SCRAM-"] {
            let text = format!("{}!", unit.repeat(40_000));
            let started = std::time::Instant::now();
            let handle = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || redact(&text, "").stdout.len())
                .unwrap();
            handle.join().unwrap();
            let took = started.elapsed();
            eprintln!("chained auth assignments redacted in {took:?}");
            assert!(
                took < std::time::Duration::from_secs(10),
                "{unit}: {took:?}"
            );
        }
    }

    #[test]
    fn slack_tokens_with_a_letter_first_body_are_redacted() {
        for token in [
            "xo\x78b-fakefakefakefakefake",
            "xo\x78a-fakefakefakefakefake",
            "xo\x78r-fakefakefakefakefake",
            "xo\x78p-fakefakefakefakefake",
            "xo\x78s-fakefakefakefakefake",
            "xo\x78a-2-fakefakefakefakefake",
        ] {
            for text in [
                token.to_string(),
                format!("echo {token}"),
                format!("KEY={token}"),
                format!("{{\"t\": \"{token}\"}}"),
            ] {
                let r = redact(&text, "");
                assert!(!r.stdout.contains("fakefake"), "{text} -> {}", r.stdout);
            }
        }
        // `o`, `c` and `e` bodies start with a digit; `xoxbot-…` is no prefix.
        for text in [
            "xo\x78o-hugs-and-kisses",
            "xo\x78c-hugs-and-kisses-forever",
            "xo\x78e-hugs-and-kisses-forever",
            "xoxbot-fakefakefakefakefake",
            "xo\x78o-fakefakefakefakefake",
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
        }
    }

    #[test]
    fn spaced_and_quoted_cookie_in_a_powershell_hashtable_is_a_header() {
        for text in [
            "Invoke-WebRequest -Headers @{ Cookie = \"sid=fakeabc123\" } https://x.test\n",
            "Invoke-WebRequest -Headers @{ \"Cookie\" = \"sid=fakeabc123\" } https://x.test\n",
            "Invoke-WebRequest -Headers @{\"Cookie\" = \"sid=fakeabc123\"} https://x.test\n",
            "$h = @{ Accept = \"x\"; Cookie = \"sid=fakeabc123\" }\n",
            "\"Cookie\" = \"sid=fakeabc123\"\n",
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains("fakeabc123"), "{text} -> {}", r.stdout);
            assert!(r.rules_fired.iter().any(|n| n == "cookie_header"), "{text}");
        }
        // A multi-line hashtable: the key stands alone on an indented line.
        for text in [
            "$h = @{\n  Cookie = \"sid=fakeabc123\"\n}\n",
            "$h = @{\n  Accept = 'x'\n  Set-Cookie = \"sid=fakeabc123\"\n}\n",
            "$h = @{\n  \"Cookie\" = \"sid=fakeabc123\"\n}\n",
            "$h = @{\n\t'Cookie' = 'sid=fakeabc123'\n}\n",
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains("fakeabc123"), "{text} -> {}", r.stdout);
            assert!(r.rules_fired.iter().any(|n| n == "cookie_header"), "{text}");
        }
        // Code stays: a spaced assignment of a string to a cookie.
        for code in [
            "def f():\n    cookie = \"theme=dark\"\n",
            "  this.cookie = \"a=b\";\n",
            "document.cookie = \"a=b\";",
            "document.cookie = \"theme=dark\";",
            "obj[\"cookie\"] = \"a=b\";",
            "headers[\"Cookie\"] = \"sid=x\";",
            "let cookie = \"theme=dark\";",
        ] {
            assert_eq!(redact(code, "").stdout, code, "{code}");
        }
    }

    #[test]
    fn dotenv_value_does_not_span_lines() {
        let r = redact("API_TOKEN=\nNEXT_LINE_IS_PUBLIC\n", "");
        assert!(r.stdout.contains("NEXT_LINE_IS_PUBLIC"), "{}", r.stdout);
    }

    #[test]
    fn key_value_secrets_behind_a_diff_marker_or_a_log_prefix_are_redacted() {
        // TASK-033: `git diff | cgpt ask`, a log pasted in. Fake values only.
        for (text, secret) in [
            ("+  password: fakehunter2fake\n", "fakehunter2fake"),
            ("+password: fakehunter2fake\n", "fakehunter2fake"),
            ("-  token: fakeabcdef0123\n", "fakeabcdef0123"),
            ("+  client_secret: fake0123abcd4567\n", "fake0123abcd4567"),
            ("+    - password: fakehunter2fake\n", "fakehunter2fake"),
            ("> password: fakehunter2fake\n", "fakehunter2fake"),
            (
                "2026-10-27T12:00:01Z INFO password: fakehunter2fake\n",
                "fakehunter2fake",
            ),
            (
                "2026-10-27 12:00:01,123 WARN token: fakeabcdef0123\n",
                "fakeabcdef0123",
            ),
            (
                "2026-10-27T12:00:01.123+02:00 [main] ERROR api_key: fakeabcdef0123\n",
                "fakeabcdef0123",
            ),
            ("12:00:01 DEBUG secret: fakeabcdef0123\n", "fakeabcdef0123"),
            ("[app] token: fakeabcdef0123\n", "fakeabcdef0123"),
            (
                "[2026-10-27 12:00:01] [app] password: fakehunter2fake\n",
                "fakehunter2fake",
            ),
            ("INFO password: fakehunter2fake\n", "fakehunter2fake"),
            ("+[app] token: fakeabcdef0123\n", "fakeabcdef0123"),
            // The `=` form was already found anywhere in a line.
            (
                "2026-10-27T12:00:01Z INFO password=fakehunter2fake\n",
                "fakehunter2fake",
            ),
            ("+PASSWORD=fakehunter2fake\n", "fakehunter2fake"),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(secret), "{text} -> {}", r.stdout);
            assert!(!r.rules_fired.is_empty(), "{text}");
        }
        // The key and the prefix stay, a quoted value stays quoted.
        let r = redact("+  password: \"fakehunter2fake\"\n", "");
        assert_eq!(r.stdout, "+  password: \"«redacted:key_value_secret»\"\n");
        let r = redact("2026-10-27T12:00:01Z INFO password: fakehunter2fake\n", "");
        assert_eq!(
            r.stdout,
            "2026-10-27T12:00:01Z INFO password: «redacted:key_value_secret»\n"
        );
        // A diff of several lines: every line, no neighbour touched.
        let diff = "@@ -1,4 +1,4 @@\n   host: db.local\n-  password: fakeoldpass1\n+  password: fakenewpass2\n   port: 5432\n";
        let r = redact(diff, "");
        assert_eq!(
            r.stdout,
            "@@ -1,4 +1,4 @@\n   host: db.local\n-  password: «redacted:key_value_secret»\n+  password: «redacted:key_value_secret»\n   port: 5432\n"
        );
    }

    #[test]
    fn key_value_lines_with_a_diff_marker_or_a_log_prefix_that_are_code_stay() {
        for text in [
            "+  password: string\n",
            "-  token: null\n",
            "+  token: ${TOKEN}\n",
            "+  api_key: ${{ secrets.API_KEY }}\n",
            "+  password_min_length: 8\n",
            "+    secretName: prod-db\n",
            "-  token_count: 12\n",
            "+  password: Optional[str] = None\n",
            "+  def f(token: str, password: Optional[str]):\n",
            "+  pub password: String,\n",
            "+  let password = read_password()?;\n",
            "+  const token = req.headers.authorization;\n",
            "+++ b/token: x\n",
            "--- a/password: x\n",
            "> Invalid token: signature mismatch\n",
            "2026-10-27T12:00:01Z INFO Invalid token: signature mismatch\n",
            "2026-10-27 INFO token: null\n",
            "[app] token: string\n",
            "[app] Invalid password: too short\n",
            "INFO user logged in, password: hidden in the form\n",
            "2026-10-27T12:00:01Z INFO request done in 12ms\n",
            "info token: abc\n",
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, text);
            assert!(r.rules_fired.is_empty(), "{:?} for {text}", r.rules_fired);
        }
        // Also all of them in one text, next to the TASK-007 code lines.
        for prefix in [
            "+",
            "+ ",
            "> ",
            "- ",
            "INFO ",
            "[app] ",
            "2026-10-27T12:00:01Z INFO ",
            "[a] [b] ",
        ] {
            let all = CODE_LINES
                .iter()
                .map(|l| format!("{prefix}{l}"))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(redact(&all, "").stdout, all, "prefix {prefix:?}");
        }
    }

    #[test]
    fn accepted_by_task_033_log_prefix_is_short() {
        // accepted by TASK-033: up to 6 log fields of up to 80 characters
        // stand before the key; a longer prefix is not looked through (the
        // `=` form and quoted keys are found anywhere, as before).
        let six = "[a] [b] [c] [d] [e] [f] token: fakeabcdef0123\n";
        assert!(!redact(six, "").stdout.contains("fakeabcdef0123"));
        let seven = "[a] [b] [c] [d] [e] [f] [g] token: fakeabcdef0123\n";
        assert_eq!(redact(seven, "").stdout, seven);
        let long = format!("[{}] token: fakeabcdef0123\n", "x".repeat(200));
        assert_eq!(redact(&long, "").stdout, long);
    }

    #[test]
    fn a_token_in_a_url_carries_one_marker() {
        for (text, want) in [
            (
                "git clone https://ghp_0123456789abcdefghijABCDEFGHIJxyz@github.com/a/b",
                "git clone https://«redacted:github_token_classic»@github.com/a/b",
            ),
            (
                "git clone https://sk-0123456789abcdefghijABCDEFGHIJ@host.test/a",
                "git clone https://«redacted:openai_api_key»@host.test/a",
            ),
            (
                // A user with a token as the password: the token's own marker
                // stays, it is not marked twice.
                "git clone https://alice:ghp_0123456789abcdefghijABCDEFGHIJxyz@github.com/a/b",
                "git clone https://alice:«redacted:github_token_classic»@github.com/a/b",
            ),
            (
                "git clone https://x-access-token:ghs_0123456789abcdefghijABCDEFGHIJxyz@github.com/a/b",
                "git clone https://x-access-token:«redacted:github_app»@github.com/a/b",
            ),
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, want);
            assert_eq!(r.stdout.matches("«redacted:").count(), 1, "{}", r.stdout);
        }
        // A token as the user, then a real password: the password is not
        // left in clear; two markers side by side, none inside another.
        for (text, want) in [
            (
                "git clone https://ghp_0123456789abcdefghijABCDEFGHIJxyz:fakepass1@github.com/a/b",
                "git clone https://«redacted:github_token_classic»:«redacted:url_userinfo_password»@github.com/a/b",
            ),
            (
                "git clone https://sk-0123456789abcdefghijABCDEFGHIJ:fakepass1@host.test/a",
                "git clone https://«redacted:openai_api_key»:«redacted:url_userinfo_password»@host.test/a",
            ),
            // A password with a token in a longer word: one marker.
            (
                "git clone https://alice:pw-ghp_0123456789abcdefghijABCDEFGHIJxyz@github.com/a/b",
                "git clone https://alice:«redacted:url_userinfo_password»@github.com/a/b",
            ),
            // Two URLs on one line.
            (
                "a https://ghp_0123456789abcdefghijABCDEFGHIJxyz@h.test/x b https://bob:fakepass2@h.test/y",
                "a https://«redacted:github_token_classic»@h.test/x b https://bob:«redacted:url_userinfo_password»@h.test/y",
            ),
        ] {
            let r = redact(text, "");
            assert_eq!(r.stdout, want);
            assert!(!r.stdout.contains("fakepass"), "{}", r.stdout);
            assert!(!r.stdout.contains("«redacted:«"), "{}", r.stdout);
        }
        // A plain password is still the url rule's.
        let r = redact("https://alice:s3cr3t@git.example.com/r.git", "");
        assert_eq!(
            r.stdout,
            "https://alice:«redacted:url_userinfo_password»@git.example.com/r.git"
        );
        // No marker is ever inside another one.
        let mixed = "\
            https://ghp_0123456789abcdefghijABCDEFGHIJxyz@github.com/a/b\n\
            Authorization: Bearer ghp_0123456789abcdefghijABCDEFGHIJxyz\n\
            password: ghp_0123456789abcdefghijABCDEFGHIJxyz\n\
            TOKEN=eyJhbGciOi.eyJzdWIiOi.fakesig\n\
            +  token: sk-0123456789abcdefghijABCDEFGHIJ\n\
            cookie: sid=ghp_0123456789abcdefghijABCDEFGHIJxyz\n";
        let r = redact(mixed, "");
        assert!(!r.stdout.contains("«redacted:«"), "{}", r.stdout);
        assert!(!r.stdout.contains("0123456789abcdefghij"), "{}", r.stdout);
    }

    #[test]
    fn ascii_rules_find_their_text_next_to_non_ascii_letters() {
        // B15: the word boundary of the AWS, cookie, url and netrc rules is
        // an ASCII one, so a Cyrillic letter next to the text is no word.
        for (text, secret, rule) in [
            (
                "ключяAKIAIOSFODNN7EXAMPLE",
                "AKIAIOSFODNN7EXAMPLE",
                "aws_access_key_id",
            ),
            (
                "AKIAIOSFODNN7EXAMPLEключ",
                "AKIAIOSFODNN7EXAMPLE",
                "aws_access_key_id",
            ),
            (
                "яhttps://alice:fakepass1@h.test/x",
                "fakepass1",
                "url_userinfo_password",
            ),
            ("яcookie: sid=fakeabc123", "fakeabc123", "cookie_header"),
            (
                "яmachine h.test login bob password fakepass3\n",
                "fakepass3",
                "netrc_password",
            ),
            (
                "яdefault password fakepass4\n",
                "fakepass4",
                "netrc_password",
            ),
        ] {
            let r = redact(text, "");
            assert!(!r.stdout.contains(secret), "{text}: {}", r.stdout);
            assert!(
                r.rules_fired.iter().any(|n| n == rule),
                "{text}: {:?}",
                r.rules_fired
            );
        }
        // The same text with a marker and Cyrillic on the line.
        let mixed = "ключ ghp_0123456789abcdefghijABCDEFGHIJxyz я \
            https://alice:fakepass1@h.test/x AKIAIOSFODNN7EXAMPLE";
        let r = redact(mixed, "");
        assert!(!r.stdout.contains("fakepass1"), "{}", r.stdout);
        assert!(!r.stdout.contains("AKIAIOSFODNN7EXAMPLE"), "{}", r.stdout);
        assert!(!r.stdout.contains("0123456789abcdefghij"), "{}", r.stdout);
        assert!(!r.stdout.contains("«redacted:«"), "{}", r.stdout);
        // An ASCII word character next to the text still keeps it: the
        // boundary is the same, only the alphabet is narrower.
        for text in [
            "xAKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLEx",
            "xcookie: sid=fakeabc123",
            "xmachine h.test login bob password fakepass3\n",
        ] {
            let r = redact(text, "");
            assert!(
                !r.rules_fired.iter().any(|n| matches!(
                    n.as_str(),
                    "aws_access_key_id"
                        | "url_userinfo_password"
                        | "cookie_header"
                        | "netrc_password"
                )),
                "{text}: {:?}",
                r.rules_fired
            );
        }
    }

    #[test]
    fn diff_and_log_prefixes_stay_linear_on_long_lines() {
        // TASK-033: the line start of `key_value_secret` takes a prefix; it
        // must not cost more than a linear pass.
        let texts: Vec<(&str, String)> = vec![
            ("one line, brackets", "[a] ".repeat(500_000)),
            ("one line, unclosed bracket", "[a".repeat(1_000_000)),
            ("one line, levels", "INFO ".repeat(400_000)),
            ("one line, dates", "2026-10-27 12:00:01 ".repeat(50_000)),
            ("one line, pairs", "+ password: x ".repeat(50_000)),
            (
                "one line, prefix, no key",
                format!("{}password", "[a] INFO ".repeat(200_000)),
            ),
            (
                "one line, prefix, key, no colon",
                format!("{}password x", "2026-10-27 INFO ".repeat(100_000)),
            ),
            (
                "many diff lines",
                "+  password: fakehunter2fake\n".repeat(20_000),
            ),
            (
                "many log lines",
                "2026-10-27T12:00:01Z INFO [app] token: fake0123456789\n".repeat(12_000),
            ),
            ("many kept lines", "+  token: string\n".repeat(40_000)),
            (
                "many plain lines",
                "[app] INFO nothing to see here\n".repeat(60_000),
            ),
            (
                "one line, tokens in urls",
                "https://ghp_0123456789abcdefghijABCDEFGHIJxyz@h.test/ ".repeat(10_000),
            ),
        ];
        for (name, text) in texts {
            let started = std::time::Instant::now();
            let handle = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || redact(&text, "").stdout.len())
                .unwrap();
            handle.join().unwrap();
            let took = started.elapsed();
            eprintln!("prefix rules, {name}: {took:?}");
            // A backstop, far above a linear pass (the slowest input takes
            // a few seconds in a debug build, 0.1 s in release) and far
            // below the minutes a quadratic one would need.
            assert!(
                took < std::time::Duration::from_secs(30),
                "{name}: {took:?}"
            );
        }
    }

    #[test]
    fn no_match_yields_empty_rules_fired() {
        let r = redact("hello world", "");
        assert!(r.rules_fired.is_empty());
        assert_eq!(r.stdout, "hello world");
    }

    #[test]
    fn truncate_summary_caps_to_head() {
        let big = "x".repeat(SUMMARY_KEEP_HEAD * 2);
        let out = truncate(&big, "", SendOutput::Summary);
        assert!(out.truncated);
        assert!(out.stdout.starts_with(&"x".repeat(SUMMARY_KEEP_HEAD)));
        assert!(out.stdout.contains("bytes elided"));
    }

    #[test]
    fn truncate_truncated_keeps_head_and_tail() {
        let head = "H".repeat(TRUNC_KEEP_HEAD);
        let mid = "M".repeat(50_000);
        let tail = "T".repeat(TRUNC_KEEP_TAIL);
        let s = format!("{}{}{}", head, mid, tail);
        let out = truncate(&s, "", SendOutput::Truncated);
        assert!(out.truncated);
        assert!(out.stdout.starts_with(&head));
        assert!(out.stdout.ends_with(&tail));
        assert!(out.stdout.contains("bytes elided"));
    }

    #[test]
    fn truncate_full_downgrades_when_over_cap() {
        let big = "x".repeat(FULL_TOTAL_HARD_CAP + 100);
        let out = truncate(&big, "", SendOutput::Full);
        assert!(out.truncated);
        assert!(matches!(out.effective_mode, SendOutput::Truncated));
    }

    #[test]
    fn truncate_full_unchanged_when_small() {
        let out = truncate("small", "", SendOutput::Full);
        assert!(!out.truncated);
        assert!(matches!(out.effective_mode, SendOutput::Full));
        assert_eq!(out.stdout, "small");
    }
}
