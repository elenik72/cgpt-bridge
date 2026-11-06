//! Gateway configuration: `<data_dir>/cgpt-gateway/config.json`, created with
//! a random bearer token on first start.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 8765;
pub const DEFAULT_TURN_TIMEOUT_MS: u64 = 900_000;
/// The longest turn timeout taken from the file: a day is more than any
/// turn needs, and deadlines far beyond it could overflow `Instant`.
pub const MAX_TURN_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
/// A client that left is noticed by a failed keepalive write. After a clean
/// close (FIN) the first write still succeeds and only provokes a reset, the
/// second fails: the worst case is two intervals (about 4 s), the best one
/// (reset at once) one.
pub const DEFAULT_KEEPALIVE_MS: u64 = 2_000;
/// How long a streamed request may still fail with a plain HTTP error before
/// the SSE stream (and with it a 200) is started.
pub const DEFAULT_FIRST_BYTE_WAIT_MS: u64 = 500;

/// A turn is not sent to ChatGPT with less budget than this left after
/// queueing: it would only be cut off mid-answer.
pub const DEFAULT_MIN_ASK_BUDGET_MS: u64 = 5_000;
/// Shortest bearer token accepted from the config file.
const MIN_TOKEN_CHARS: usize = 32;
/// Turns of different sessions that may run at once, at most (the extension's
/// `own_tabs` may set a lower limit).
pub const DEFAULT_MAX_PARALLEL: u32 = 3;
/// The range `max_parallel` is clamped to.
pub const MIN_MAX_PARALLEL: u32 = 1;
pub const MAX_MAX_PARALLEL: u32 = 8;
/// The most turns `debug_capture_turns` may capture.
pub const MAX_DEBUG_CAPTURE_TURNS: u32 = 100;
/// The keys of the file: the fields of [`Config`] that are not `serde(skip)`.
const KNOWN_KEYS: [&str; 6] = [
    "port",
    "token",
    "turn_timeout_ms",
    "allow_tools",
    "max_parallel",
    "debug_capture_turns",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    pub token: String,
    #[serde(default = "default_turn_timeout_ms")]
    pub turn_timeout_ms: u64,
    /// Tools the user allowed ChatGPT to use beyond the safe set (`execute`).
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Turns of different sessions that may run at once (see
    /// [`DEFAULT_MAX_PARALLEL`]); clamped to 1..=8 when read.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
    /// Debug aid, off (0) by default and left out of a new file: the raw
    /// answer streams of the next this many turns after the start are
    /// written, redacted, to `captures/` (see `journal`). Only this file
    /// turns it on, never a request or ChatGPT; at most
    /// [`MAX_DEBUG_CAPTURE_TURNS`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub debug_capture_turns: u32,
    /// Interval of the `: keepalive` SSE comment. Not part of the file; tests
    /// shorten it.
    #[serde(skip, default = "default_keepalive_ms")]
    pub keepalive_ms: u64,
    /// See [`DEFAULT_FIRST_BYTE_WAIT_MS`]. Not part of the file.
    #[serde(skip, default = "default_first_byte_wait_ms")]
    pub first_byte_wait_ms: u64,
    /// See [`DEFAULT_MIN_ASK_BUDGET_MS`]. Not part of the file.
    #[serde(skip, default = "default_min_ask_budget_ms")]
    pub min_ask_budget_ms: u64,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_turn_timeout_ms() -> u64 {
    DEFAULT_TURN_TIMEOUT_MS
}
fn default_max_parallel() -> u32 {
    DEFAULT_MAX_PARALLEL
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}
fn default_keepalive_ms() -> u64 {
    DEFAULT_KEEPALIVE_MS
}
fn default_first_byte_wait_ms() -> u64 {
    DEFAULT_FIRST_BYTE_WAIT_MS
}
fn default_min_ask_budget_ms() -> u64 {
    DEFAULT_MIN_ASK_BUDGET_MS
}

impl Config {
    pub fn with_token(token: String) -> Config {
        Config {
            port: DEFAULT_PORT,
            token,
            turn_timeout_ms: DEFAULT_TURN_TIMEOUT_MS,
            allow_tools: Vec::new(),
            max_parallel: DEFAULT_MAX_PARALLEL,
            debug_capture_turns: 0,
            keepalive_ms: DEFAULT_KEEPALIVE_MS,
            first_byte_wait_ms: DEFAULT_FIRST_BYTE_WAIT_MS,
            min_ask_budget_ms: DEFAULT_MIN_ASK_BUDGET_MS,
        }
    }

    /// [`Config::min_ask_budget_ms`] as applied: a minimum above half the
    /// timeout would refuse every turn.
    pub fn effective_min_ask_budget_ms(&self) -> u64 {
        self.min_ask_budget_ms.min(self.turn_timeout_ms / 2)
    }
}

/// 32 random bytes as 64 lowercase hex characters.
pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Load `<dir>/config.json`, or create it with defaults and a fresh token.
/// The second value holds the warnings (unknown keys, a `max_parallel` out of
/// range), for the caller to log once the gateway's start line is out.
pub fn load_or_create(dir: &Path) -> io::Result<(Config, Vec<String>)> {
    let path = dir.join("config.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let invalid = |why: String| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {}", path.display(), why),
                )
            };
            let mut cfg: Config =
                serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
            cfg.turn_timeout_ms = cfg.turn_timeout_ms.min(MAX_TURN_TIMEOUT_MS);
            if cfg.token.chars().count() < MIN_TOKEN_CHARS {
                return Err(invalid(format!(
                    "\"token\" must be at least {} characters (delete the file to get a fresh one)",
                    MIN_TOKEN_CHARS
                )));
            }
            let warnings = unknown_keys_warning(&path, &text)
                .into_iter()
                .chain(clamp_max_parallel(&mut cfg, &path))
                .chain(clamp_debug_capture_turns(&mut cfg, &path))
                .collect();
            Ok((cfg, warnings))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let cfg = Config::with_token(generate_token()?);
            std::fs::create_dir_all(dir)?;
            write_private(&path, &serde_json::to_string_pretty(&cfg)?)?;
            Ok((cfg, Vec::new()))
        }
        Err(e) => Err(e),
    }
}

/// Bring `max_parallel` into 1..=8; the log line when it was outside.
fn clamp_max_parallel(cfg: &mut Config, path: &Path) -> Option<String> {
    let given = cfg.max_parallel;
    cfg.max_parallel = given.clamp(MIN_MAX_PARALLEL, MAX_MAX_PARALLEL);
    (cfg.max_parallel != given).then(|| {
        format!(
            "cgpt-gateway: {}: \"max_parallel\" {} is outside {}..={}, {} is used",
            path.display(),
            given,
            MIN_MAX_PARALLEL,
            MAX_MAX_PARALLEL,
            cfg.max_parallel
        )
    })
}

/// Bring `debug_capture_turns` to at most [`MAX_DEBUG_CAPTURE_TURNS`]; the
/// log line when it was more.
fn clamp_debug_capture_turns(cfg: &mut Config, path: &Path) -> Option<String> {
    let given = cfg.debug_capture_turns;
    cfg.debug_capture_turns = given.min(MAX_DEBUG_CAPTURE_TURNS);
    (cfg.debug_capture_turns != given).then(|| {
        format!(
            "cgpt-gateway: {}: \"debug_capture_turns\" {} is more than {}, {} is used",
            path.display(),
            given,
            MAX_DEBUG_CAPTURE_TURNS,
            cfg.debug_capture_turns
        )
    })
}

/// Most unknown keys named in the warning, and the longest key name shown.
const MAX_LISTED_KEYS: usize = 10;
const MAX_KEY_CHARS: usize = 64;

/// The log line for the top-level keys of the file that are not in
/// [`KNOWN_KEYS`] (a typo such as `allowed_tools` is otherwise ignored without
/// a word), `None` when there are none. The gateway starts either way. Names
/// are cut to [`MAX_KEY_CHARS`] and listed up to [`MAX_LISTED_KEYS`], and are
/// escaped (`{:?}`), so the line stays one short line whatever the file holds.
fn unknown_keys_warning(path: &Path, text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut unknown: Vec<&str> = value
        .as_object()?
        .keys()
        .map(String::as_str)
        .filter(|k| !KNOWN_KEYS.contains(k))
        .collect();
    if unknown.is_empty() {
        return None;
    }
    unknown.sort();
    let quote = |key: &str| {
        let mut shown: String = key.chars().take(MAX_KEY_CHARS).collect();
        if key.chars().nth(MAX_KEY_CHARS).is_some() {
            shown.push('…');
        }
        format!("{:?}", shown)
    };
    let mut listed = unknown
        .iter()
        .take(MAX_LISTED_KEYS)
        .map(|k| quote(k))
        .collect::<Vec<_>>()
        .join(", ");
    if unknown.len() > MAX_LISTED_KEYS {
        listed.push_str(&format!(" and {} more", unknown.len() - MAX_LISTED_KEYS));
    }
    let known = KNOWN_KEYS.map(quote).join(", ");
    Some(format!(
        "cgpt-gateway: {}: unknown key(s) ignored: {} (known: {})",
        path.display(),
        listed,
        known
    ))
}

/// The file holds the bearer token: readable by the owner only on Unix.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

/// `<data_dir>/cgpt-gateway`. Windows: `%APPDATA%`; macOS:
/// `~/Library/Application Support`; elsewhere `$XDG_DATA_HOME` or
/// `~/.local/share`.
pub fn gateway_dir() -> io::Result<PathBuf> {
    gateway_dir_with(&|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

fn gateway_dir_with(env: &dyn Fn(&str) -> Option<String>) -> io::Result<PathBuf> {
    let missing =
        |name: &str| io::Error::new(io::ErrorKind::NotFound, format!("${} is not set", name));
    let base = if cfg!(windows) {
        PathBuf::from(env("APPDATA").ok_or_else(|| missing("APPDATA"))?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(env("HOME").ok_or_else(|| missing("HOME"))?)
            .join("Library")
            .join("Application Support")
    } else if let Some(xdg) = env("XDG_DATA_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(env("HOME").ok_or_else(|| missing("HOME"))?)
            .join(".local")
            .join("share")
    };
    Ok(base.join("cgpt-gateway"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cgpt-gw-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn keepalive_default_is_2_seconds() {
        // A client that left is noticed at the next keepalive write: this is
        // the latency of a stop from OpenCode.
        assert_eq!(DEFAULT_KEEPALIVE_MS, 2_000);
    }

    #[test]
    fn config_created_with_random_token_and_defaults() {
        let dir = temp_dir("cfg");
        let cfg = load_or_create(&dir).unwrap().0;
        assert_eq!(cfg.token.len(), 64);
        assert!(cfg
            .token
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(cfg.port, 8765);
        assert_eq!(cfg.turn_timeout_ms, 900_000);
        assert!(cfg.allow_tools.is_empty());
        assert_eq!(cfg.max_parallel, 3);
        assert_eq!(cfg.keepalive_ms, 2_000);
        assert_eq!(cfg.first_byte_wait_ms, 500);
        assert_eq!(cfg.min_ask_budget_ms, 5_000);

        // The file is written once; a second start reads the same token.
        let again = load_or_create(&dir).unwrap().0;
        assert_eq!(again.token, cfg.token);

        // Keys on disk are exactly the documented ones.
        let text = std::fs::read_to_string(dir.join("config.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "allow_tools",
                "max_parallel",
                "port",
                "token",
                "turn_timeout_ms"
            ]
        );

        // A different dir gets a different token.
        let other = load_or_create(&temp_dir("cfg2")).unwrap().0;
        assert_ne!(other.token, cfg.token);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(temp_dir("cfg2"));
    }

    #[test]
    fn partial_config_file_gets_defaults() {
        let dir = temp_dir("partial");
        std::fs::create_dir_all(&dir).unwrap();
        let token = "a".repeat(64);
        std::fs::write(
            dir.join("config.json"),
            format!(r#"{{"token":"{}","allow_tools":["execute"]}}"#, token),
        )
        .unwrap();
        let cfg = load_or_create(&dir).unwrap().0;
        assert_eq!(cfg.token, token);
        assert_eq!(cfg.port, 8765);
        assert_eq!(cfg.turn_timeout_ms, 900_000);
        // The non-file fields have their defaults after deserialization.
        assert_eq!(cfg.keepalive_ms, 2_000);
        assert_eq!(cfg.first_byte_wait_ms, 500);
        assert_eq!(cfg.min_ask_budget_ms, 5_000);
        assert_eq!(cfg.allow_tools, ["execute"]);
        // A file written before `max_parallel` existed.
        assert_eq!(cfg.max_parallel, 3);
        // Debug capture is off unless the file says otherwise.
        assert_eq!(cfg.debug_capture_turns, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_capture_turns_is_read_and_capped_with_a_warning() {
        let dir = temp_dir("debug-capture");
        std::fs::create_dir_all(&dir).unwrap();
        for (given, used, warned) in [
            (0, 0, false),
            (3, 3, false),
            (100, 100, false),
            (5000, 100, true),
        ] {
            std::fs::write(
                dir.join("config.json"),
                format!(
                    r#"{{"token":"{}","debug_capture_turns":{}}}"#,
                    "a".repeat(64),
                    given
                ),
            )
            .unwrap();
            let (cfg, warnings) = load_or_create(&dir).unwrap();
            assert_eq!(cfg.debug_capture_turns, used, "{given}");
            assert_eq!(warnings.len(), usize::from(warned), "{:?}", warnings);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_parallel_is_read_and_clamped_to_1_to_8_with_a_warning() {
        let dir = temp_dir("max-parallel");
        std::fs::create_dir_all(&dir).unwrap();
        for (given, used, warned) in [
            (5, 5, false),
            (1, 1, false),
            (8, 8, false),
            (0, 1, true),
            (9, 8, true),
            (1000, 8, true),
        ] {
            std::fs::write(
                dir.join("config.json"),
                format!(
                    r#"{{"token":"{}","max_parallel":{}}}"#,
                    "a".repeat(64),
                    given
                ),
            )
            .unwrap();
            let (cfg, warnings) = load_or_create(&dir).unwrap();
            assert_eq!(cfg.max_parallel, used, "{given}");
            assert_eq!(
                warnings.len(),
                usize::from(warned),
                "{given}: {:?}",
                warnings
            );
            if warned {
                assert!(
                    warnings[0].contains(&format!(
                        "\"max_parallel\" {} is outside 1..=8, {} is used",
                        given, used
                    )),
                    "{}",
                    warnings[0]
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn huge_turn_timeout_is_cut_to_a_day() {
        let dir = temp_dir("huge-timeout");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            format!(
                r#"{{"token":"{}","turn_timeout_ms":{}}}"#,
                "a".repeat(64),
                u64::MAX
            ),
        )
        .unwrap();
        let cfg = load_or_create(&dir).unwrap().0;
        assert_eq!(cfg.turn_timeout_ms, 24 * 60 * 60 * 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_token_is_rejected() {
        let dir = temp_dir("short");
        std::fs::create_dir_all(&dir).unwrap();
        for token in ["", "abc", &"a".repeat(31)] {
            std::fs::write(
                dir.join("config.json"),
                format!(r#"{{"token":"{}"}}"#, token),
            )
            .unwrap();
            let err = load_or_create(&dir).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains("at least 32"), "{}", err);
        }
        std::fs::write(
            dir.join("config.json"),
            format!(r#"{{"token":"{}"}}"#, "a".repeat(32)),
        )
        .unwrap();
        assert!(load_or_create(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn data_dir_macos() {
        let env = |n: &str| (n == "HOME").then(|| "/Users/a".to_string());
        assert_eq!(
            gateway_dir_with(&env).unwrap(),
            PathBuf::from("/Users/a/Library/Application Support/cgpt-gateway")
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn data_dir_linux() {
        let home_only = |n: &str| (n == "HOME").then(|| "/home/a".to_string());
        assert_eq!(
            gateway_dir_with(&home_only).unwrap(),
            PathBuf::from("/home/a/.local/share/cgpt-gateway")
        );
        let xdg = |n: &str| (n == "XDG_DATA_HOME").then(|| "/x".to_string());
        assert_eq!(
            gateway_dir_with(&xdg).unwrap(),
            PathBuf::from("/x/cgpt-gateway")
        );
    }

    #[cfg(windows)]
    #[test]
    fn data_dir_windows() {
        let env = |n: &str| (n == "APPDATA").then(|| r"C:\Users\a\AppData\Roaming".to_string());
        assert_eq!(
            gateway_dir_with(&env).unwrap(),
            PathBuf::from(r"C:\Users\a\AppData\Roaming").join("cgpt-gateway")
        );
    }

    #[test]
    fn a_misspelled_key_is_named_in_a_warning_and_the_file_still_loads() {
        let dir = temp_dir("unknown");
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!(
            r#"{{"token":"{}","allowed_tools":["execute"]}}"#,
            "a".repeat(64)
        );
        std::fs::write(dir.join("config.json"), &text).unwrap();
        let (cfg, warnings) = load_or_create(&dir).unwrap();
        // The typo changed nothing: the real key keeps its default.
        assert!(cfg.allow_tools.is_empty());
        let [warning] = warnings.as_slice() else {
            panic!("one warning: {:?}", warnings)
        };
        assert_eq!(
            Some(warning.clone()),
            unknown_keys_warning(&dir.join("config.json"), &text)
        );
        assert_eq!(
            *warning,
            format!(
                "cgpt-gateway: {}: unknown key(s) ignored: \"allowed_tools\" \
                 (known: \"port\", \"token\", \"turn_timeout_ms\", \"allow_tools\", \
                 \"max_parallel\", \"debug_capture_turns\")",
                dir.join("config.json").display()
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_unknown_key_is_listed_sorted_and_a_key_not_read_from_the_file_is_unknown() {
        let text = r#"{"token":"x","port":1,"zeta":1,"alpha":2,"keepalive_ms":5}"#;
        let warning = unknown_keys_warning(Path::new("config.json"), text).unwrap();
        assert!(
            warning.contains("\"alpha\", \"keepalive_ms\", \"zeta\""),
            "{warning}"
        );
    }

    #[test]
    fn a_correct_config_gives_no_warning() {
        let dir = temp_dir("known");
        assert!(load_or_create(&dir).unwrap().1.is_empty());
        let text = std::fs::read_to_string(dir.join("config.json")).unwrap();
        // Read back (not created): still no warning.
        assert!(load_or_create(&dir).unwrap().1.is_empty());
        assert_eq!(unknown_keys_warning(&dir.join("config.json"), &text), None);
        // The known keys are the ones a config the gateway writes has.
        let written: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<&str> = written
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        // ...but for the debug aid, which a new file leaves out (off).
        let mut known: Vec<&str> = KNOWN_KEYS
            .into_iter()
            .filter(|k| *k != "debug_capture_turns")
            .collect();
        known.sort();
        assert_eq!(keys, known);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_with_control_characters_stays_on_one_escaped_line() {
        let text = r#"{"token":"x","a\nb\r\u001b[31m\u0007c":1}"#;
        let warning = unknown_keys_warning(Path::new("config.json"), text).unwrap();
        assert!(
            !warning.chars().any(|c| c.is_control()),
            "no raw control character: {warning:?}"
        );
        assert!(warning.contains(r#""a\nb\r\u{1b}[31m\u{7}c""#), "{warning}");
    }

    #[test]
    fn a_very_long_key_is_cut_and_marked() {
        let text = format!(r#"{{"token":"x","{}":1}}"#, "k".repeat(10_000));
        let warning = unknown_keys_warning(Path::new("config.json"), &text).unwrap();
        assert!(
            warning.contains(&format!("\"{}…\"", "k".repeat(MAX_KEY_CHARS))),
            "{warning}"
        );
        assert!(
            !warning.contains(&"k".repeat(MAX_KEY_CHARS + 1)),
            "{warning}"
        );
        assert!(warning.len() < 400, "{}", warning.len());
        // A key of exactly the limit is shown whole.
        let text = format!(r#"{{"{}":1}}"#, "k".repeat(MAX_KEY_CHARS));
        let warning = unknown_keys_warning(Path::new("config.json"), &text).unwrap();
        assert!(
            warning.contains(&format!("\"{}\"", "k".repeat(MAX_KEY_CHARS))),
            "{warning}"
        );
    }

    #[test]
    fn many_unknown_keys_are_listed_up_to_the_limit_and_counted() {
        let keys: Vec<String> = (0..25).map(|i| format!("\"k{:02}\":1", i)).collect();
        let text = format!("{{{}}}", keys.join(","));
        let warning = unknown_keys_warning(Path::new("config.json"), &text).unwrap();
        assert!(warning.contains(r#""k00", "k01""#), "{warning}");
        assert!(warning.contains(r#""k09" and 15 more"#), "{warning}");
        assert!(!warning.contains("k10"), "{warning}");
    }

    #[test]
    fn a_file_that_is_not_an_object_gives_the_old_error_and_no_warning() {
        let dir = temp_dir("notobject");
        std::fs::create_dir_all(&dir).unwrap();
        for text in ["[]", "\"token\"", "null", "5", "[{\"zzz\":1}]"] {
            std::fs::write(dir.join("config.json"), text).unwrap();
            assert_eq!(unknown_keys_warning(&dir.join("config.json"), text), None);
            let err = load_or_create(&dir).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{text}");
            assert!(err.to_string().contains("config.json"), "{err}");
        }
        std::fs::write(dir.join("config.json"), "{not json").unwrap();
        assert_eq!(
            unknown_keys_warning(&dir.join("config.json"), "{not json"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
