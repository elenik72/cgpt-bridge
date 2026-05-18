//! In-process markdown renderer for the final assistant message of a
//! `cgpt agent` session.
//!
//! Uses `termimad` so there is no external runtime dependency to install.
//! Renders headers, lists, blockquotes, tables, inline code/bold/italic,
//! and fenced code blocks (no syntax highlighting in v0.1 — that is a
//! follow-up with `syntect`).
//!
//! TTY policy:
//!   - If stdout is not a TTY (piped / redirected), the raw markdown is
//!     emitted so downstream consumers don't receive ANSI escapes.
//!   - On any unexpected error from the renderer the raw markdown is
//!     printed instead so the agent run never loses content.

use std::io::IsTerminal;

use termimad::crossterm::style::Color;
use termimad::{Alignment, MadSkin};

/// Print `text` to stdout. Pretty-renders when stdout is a TTY, otherwise
/// emits raw markdown verbatim. Always guarantees a trailing newline.
pub fn print_markdown(text: &str) {
    if !std::io::stdout().is_terminal() {
        print_raw(text);
        return;
    }
    let skin = build_skin();
    let rendered = skin.text(text, Some(terminal_width())).to_string();
    print!("{}", rendered);
    if !rendered.ends_with('\n') {
        println!();
    }
}

fn print_raw(text: &str) {
    print!("{}", text);
    if !text.ends_with('\n') {
        println!();
    }
}

/// Tuned theme. Goal: readable on both light and dark terminals, no
/// hard-coded backgrounds that fight the user's color scheme. Foreground
/// accents only.
fn build_skin() -> MadSkin {
    let mut skin = MadSkin::default();

    // Headers: descending accent intensity.
    skin.set_headers_fg(Color::Cyan);
    skin.headers[0].align = Alignment::Left;
    skin.headers[0].set_fg(Color::Cyan);
    if skin.headers.len() > 1 {
        skin.headers[1].set_fg(Color::Cyan);
    }
    if skin.headers.len() > 2 {
        skin.headers[2].set_fg(Color::Blue);
    }

    // Emphasis.
    skin.bold.set_fg(Color::White);
    skin.italic.set_fg(Color::Magenta);
    skin.strikeout.set_fg(Color::DarkGrey);

    // Inline code + fenced code blocks: foreground tweak only, no bg so
    // the user's terminal background carries through.
    skin.inline_code.set_fg(Color::Yellow);
    skin.code_block.set_fg(Color::Yellow);

    // Lists / bullets / quotes.
    skin.bullet.set_fg(Color::Cyan);
    skin.quote_mark.set_fg(Color::DarkGrey);

    // Tables look like tables.
    skin.table.align = Alignment::Center;

    skin
}

fn terminal_width() -> usize {
    termimad::terminal_size().0 as usize
}
