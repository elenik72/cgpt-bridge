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
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| build_rules())
}

fn build_rules() -> Vec<Rule> {
    let mut v = Vec::new();
    let add = |v: &mut Vec<Rule>, name: &'static str, pat: &str| {
        v.push(Rule {
            name,
            re: Regex::new(pat).expect("redaction regex must compile"),
        });
    };

    // Multi-line private key blocks first — these are the loudest signal and
    // we want to redact them whole, not piece by piece.
    add(
        &mut v,
        "private_key_block",
        r"(?s)-----BEGIN (?:RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----.*?-----END (?:RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----",
    );
    add(
        &mut v,
        "pgp_private_key_block",
        r"(?s)-----BEGIN PGP PRIVATE KEY BLOCK-----.*?-----END PGP PRIVATE KEY BLOCK-----",
    );

    // Vendor API keys with stable prefixes.
    add(
        &mut v,
        "openai_api_key",
        r"sk-(?:proj-)?[A-Za-z0-9_\-]{20,}",
    );
    add(&mut v, "anthropic_api_key", r"sk-ant-[A-Za-z0-9_\-]{20,}");
    add(&mut v, "github_token_classic", r"ghp_[A-Za-z0-9]{30,}");
    add(
        &mut v,
        "github_token_fine_grained",
        r"github_pat_[A-Za-z0-9_]{20,}",
    );
    add(&mut v, "github_oauth", r"gho_[A-Za-z0-9]{30,}");
    add(&mut v, "github_app", r"(?:ghu|ghs|ghr)_[A-Za-z0-9]{30,}");
    add(
        &mut v,
        "aws_access_key_id",
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    );
    add(&mut v, "slack_token", r"xox[abprs]-[A-Za-z0-9\-]{10,}");
    add(&mut v, "npm_token", r"npm_[A-Za-z0-9]{30,}");
    add(&mut v, "stripe_live_secret", r"sk_live_[A-Za-z0-9]{20,}");

    // JWT-like (three base64url-ish segments separated by dots, starts eyJ).
    add(
        &mut v,
        "jwt_like",
        r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
    );

    // Bearer / Basic auth headers.
    add(
        &mut v,
        "generic_bearer_token",
        r"(?i)Authorization:\s*Bearer\s+[A-Za-z0-9._\-]+",
    );
    add(
        &mut v,
        "generic_basic_auth",
        r"(?i)Authorization:\s*Basic\s+[A-Za-z0-9+/=]+",
    );

    // .env-style assignments. We replace only the value, not the key, so the
    // structure remains readable.
    add(
        &mut v,
        "dotenv_secret_assignment",
        r"(?im)^(.*?(?:SECRET|TOKEN|PASSWORD|PASSWD|API_KEY|APIKEY|PRIVATE_KEY)[A-Z0-9_]*\s*=\s*)(\S.*)$",
    );

    v
}

pub fn redact(stdout: &str, stderr: &str) -> Redaction {
    let mut fired = Vec::new();
    let mut out = stdout.to_string();
    let mut err = stderr.to_string();

    for rule in rules() {
        let placeholder = format!("«redacted:{}»", rule.name);
        let (after_out, n_out) = replace_all_counted(&rule.re, &out, &placeholder, rule.name);
        let (after_err, n_err) = replace_all_counted(&rule.re, &err, &placeholder, rule.name);
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

fn replace_all_counted(
    re: &Regex,
    text: &str,
    placeholder: &str,
    rule_name: &str,
) -> (String, usize) {
    let mut count = 0usize;
    // For the dotenv rule we preserve the key (capture group 1) and replace
    // only the value (capture group 2).
    if rule_name == "dotenv_secret_assignment" {
        let out = re.replace_all(text, |caps: &regex::Captures| {
            count += 1;
            format!("{}{}", &caps[1], placeholder)
        });
        return (out.into_owned(), count);
    }
    let out = re.replace_all(text, |_: &regex::Captures| {
        count += 1;
        placeholder.to_string()
    });
    (out.into_owned(), count)
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
