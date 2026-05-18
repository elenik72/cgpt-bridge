//! In-process markdown renderer for the final assistant message of a
//! `cgpt agent` session.
//!
//! Layout strategy:
//!   - Non-code-block prose is rendered through `termimad` with a tuned skin
//!     (headers, lists, tables, emphasis, blockquote).
//!   - Fenced code blocks are rendered through `syntect` so the code itself
//!     is syntax-highlighted. Language is read from the info string after
//!     the opening fence (```rust → Rust, ```sh → Shell, etc). Unknown or
//!     missing language falls back to plain text (still nicely framed).
//!
//! TTY policy:
//!   - Non-TTY stdout → raw markdown emitted verbatim. Downstream consumers
//!     get clean text instead of ANSI escapes.

use std::io::IsTerminal;
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;
use termimad::crossterm::style::Color;
use termimad::{Alignment, MadSkin};

/// Print `text` to stdout. Pretty-renders when stdout is a TTY, otherwise
/// emits raw markdown verbatim. Always guarantees a trailing newline.
pub fn print_markdown(text: &str) {
    if !std::io::stdout().is_terminal() {
        print_raw(text);
        return;
    }
    let out = render_to_string(text);
    print!("{}", out);
    if !out.ends_with('\n') {
        println!();
    }
}

fn print_raw(text: &str) {
    print!("{}", text);
    if !text.ends_with('\n') {
        println!();
    }
}

fn render_to_string(text: &str) -> String {
    let skin = build_skin();
    let width = terminal_width();
    let segments = split_into_segments(text);
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Prose(s) => {
                let rendered = skin.text(&s, Some(width)).to_string();
                out.push_str(&rendered);
            }
            Segment::Code { lang, body } => {
                out.push_str(&highlight_code_block(&lang, &body));
            }
        }
    }
    out
}

enum Segment {
    Prose(String),
    Code { lang: String, body: String },
}

/// Walk the markdown line by line, separating fenced code blocks from
/// non-code prose. Recognises ``` and ~~~ fences with optional language
/// info string. A fence must start at column 0 (after optional spaces but
/// not inside another block), matching what termimad treats as a code
/// fence. This is intentionally tolerant: malformed markdown still renders.
fn split_into_segments(text: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut prose = String::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_body = String::new();
    let mut fence_marker = "";

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.trim_end_matches('\n');
        let trimmed = line.trim_start();

        if !in_code {
            if let Some(marker) = detect_fence_open(trimmed) {
                if !prose.is_empty() {
                    out.push(Segment::Prose(std::mem::take(&mut prose)));
                }
                fence_marker = marker;
                code_lang = trimmed[marker.len()..].trim().to_string();
                code_body.clear();
                in_code = true;
                continue;
            }
            prose.push_str(raw_line);
        } else if trimmed.starts_with(fence_marker)
            && trimmed[fence_marker.len()..].trim().is_empty()
        {
            // Closing fence.
            out.push(Segment::Code {
                lang: std::mem::take(&mut code_lang),
                body: std::mem::take(&mut code_body),
            });
            in_code = false;
            fence_marker = "";
        } else {
            code_body.push_str(raw_line);
        }
    }

    if in_code {
        // Unterminated fence: render whatever we have as code anyway, so
        // the user still sees the content.
        out.push(Segment::Code {
            lang: code_lang,
            body: code_body,
        });
    } else if !prose.is_empty() {
        out.push(Segment::Prose(prose));
    }
    out
}

fn detect_fence_open(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn highlight_code_block(lang: &str, body: &str) -> String {
    let (ss, theme) = syntect_assets();
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = String::new();
    // Leading blank line separates from prose above.
    out.push('\n');
    for line in syntect::util::LinesWithEndings::from(body) {
        match h.highlight_line(line, ss) {
            Ok(ranges) => {
                let escaped: String = as_24_bit_terminal_escaped(&ranges[..], false);
                out.push_str(&escaped);
            }
            Err(_) => out.push_str(line),
        }
    }
    // Reset SGR so termimad output after us starts clean.
    out.push_str("\x1b[0m");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn syntect_assets() -> (&'static SyntaxSet, &'static Theme) {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    static TH: OnceLock<Theme> = OnceLock::new();
    let ss = SS.get_or_init(SyntaxSet::load_defaults_newlines);
    let theme = TH.get_or_init(|| {
        let ts = ThemeSet::load_defaults();
        // base16-ocean.dark reads well on both light- and dark-background
        // terminals. Solarized themes look washed-out on dark; Monokai
        // assumes a dark bg. base16-ocean is the safest default.
        ts.themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_else(|| ts.themes.values().next().cloned().unwrap())
    });
    (ss, theme)
}

/// Tuned skin. Goal: readable on both light and dark terminals, no
/// hard-coded backgrounds. Code blocks are NOT styled by termimad — we
/// handle them via syntect in `highlight_code_block`.
fn build_skin() -> MadSkin {
    let mut skin = MadSkin::default();

    skin.set_headers_fg(Color::Cyan);
    skin.headers[0].align = Alignment::Left;
    skin.headers[0].set_fg(Color::Cyan);
    if skin.headers.len() > 1 {
        skin.headers[1].set_fg(Color::Cyan);
    }
    if skin.headers.len() > 2 {
        skin.headers[2].set_fg(Color::Blue);
    }

    skin.bold.set_fg(Color::White);
    skin.italic.set_fg(Color::Magenta);
    skin.strikeout.set_fg(Color::DarkGrey);

    skin.inline_code.set_fg(Color::Yellow);
    // code_block kept default; in practice we strip code blocks out before
    // they reach termimad.
    skin.code_block.set_fg(Color::Yellow);

    skin.bullet.set_fg(Color::Cyan);
    skin.quote_mark.set_fg(Color::DarkGrey);

    skin.table.align = Alignment::Center;

    skin
}

fn terminal_width() -> usize {
    termimad::terminal_size().0 as usize
}

// Hold onto Style import so the `use` at the top isn't flagged as dead
// when as_24_bit_terminal_escaped is the only consumer of the type
// alias chain.
#[allow(dead_code)]
fn _style_keepalive(_: Style) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_segregates_fenced_blocks() {
        let md =
            "Hello\n\n```rust\nfn main() {}\n```\n\nMid\n\n~~~\nplain block\n~~~\n\ntail\n";
        let segs = split_into_segments(md);
        // Expect: prose, code(rust), prose, code(""), prose
        assert!(matches!(segs[0], Segment::Prose(_)));
        match &segs[1] {
            Segment::Code { lang, body } => {
                assert_eq!(lang, "rust");
                assert!(body.contains("fn main()"));
            }
            _ => panic!("expected Code"),
        }
        assert!(matches!(segs[2], Segment::Prose(_)));
        match &segs[3] {
            Segment::Code { lang, body } => {
                assert_eq!(lang, "");
                assert!(body.contains("plain block"));
            }
            _ => panic!("expected Code"),
        }
        assert!(matches!(segs[4], Segment::Prose(_)));
    }

    #[test]
    fn unterminated_fence_still_renders_as_code() {
        let md = "intro\n\n```python\nprint(1)\nprint(2)\n";
        let segs = split_into_segments(md);
        assert_eq!(segs.len(), 2);
        match &segs[1] {
            Segment::Code { lang, body } => {
                assert_eq!(lang, "python");
                assert!(body.contains("print(1)"));
            }
            _ => panic!("expected Code"),
        }
    }

    #[test]
    fn no_code_block_returns_single_prose() {
        let md = "# Title\n\nbody text\n";
        let segs = split_into_segments(md);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0], Segment::Prose(_)));
    }

    #[test]
    fn highlight_block_resets_sgr() {
        let out = highlight_code_block("rust", "fn x() {}\n");
        assert!(out.ends_with("\x1b[0m\n"));
    }
}
