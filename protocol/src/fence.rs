//! Fenced code blocks in assistant text. Fence matching follows CommonMark:
//! a fence is a run of three or more backticks or tildes (a backtick fence
//! has no backtick in its info string), and a block runs to a line of only
//! the same character, at least as many as the opening run. A block inside
//! another block is content of the outer one, not a block of its own. An
//! unclosed block runs to the end of the text.
//!
//! Unlike CommonMark, indentation before a fence line is not limited (the
//! gateway and the renderer accept any). Each block records it in
//! `Block::indent`; the agent protocol parser accepts only up to three
//! spaces, since four or more (or a tab) make an indented code sample.
//!
//! One scanner for every reader of ChatGPT's text: the agent protocol parser
//! (`cgpt-agent-response-v1`), the CLI renderer and the gateway's tool-call
//! parser.

#[cfg(any(test, feature = "test-support"))]
pub mod corpus;

/// An opening fence: a run of at least three backticks or tildes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fence {
    pub ch: char,
    pub len: usize,
}

impl Fence {
    /// The CommonMark rule: a line of only the same character, at least as
    /// many as the opening run (surrounding whitespace allowed).
    pub fn closes(&self, line: &str) -> bool {
        let t = line.trim();
        let n = t.chars().take_while(|&c| c == self.ch).count();
        n >= self.len && n == t.len()
    }
}

/// The fence and the info string of a fence-opening line.
pub fn fence_open(line: &str) -> Option<(Fence, &str)> {
    let t = line.trim_start();
    let ch = t.chars().next().filter(|&c| c == '`' || c == '~')?;
    let len = t.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    // The fence character is ASCII: `len` characters are `len` bytes.
    let info = t[len..].trim();
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((Fence { ch, len }, info))
}

/// A top-level fenced code block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Byte offset of the opening fence line.
    pub start: usize,
    /// Byte offset just past the closing fence line (its newline included),
    /// or the end of the text for an unclosed block.
    pub end: usize,
    pub fence: Fence,
    /// The whitespace before the opening fence on its line.
    pub indent: String,
    /// The info string after the opening fence, trimmed.
    pub info: String,
    /// The lines between the fences, each with its line ending.
    pub body: String,
    pub closed: bool,
}

impl Block {
    /// The first word of the info string; empty for a bare fence.
    pub fn label(&self) -> &str {
        self.info.split_whitespace().next().unwrap_or("")
    }
}

/// Every top-level fenced code block, in order. A block runs from its
/// opening fence to the matching closing fence, so a ```` block may hold
/// ``` lines. An unclosed block runs to the end of the text.
pub fn fenced_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut open: Option<Block> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        match &mut open {
            Some(block) if !block.fence.closes(line) => block.body.push_str(line),
            Some(_) => {
                if let Some(mut block) = open.take() {
                    block.end = offset;
                    block.closed = true;
                    blocks.push(block);
                }
            }
            None => {
                if let Some((fence, info)) = fence_open(line) {
                    let indent = &line[..line.len() - line.trim_start().len()];
                    open = Some(Block {
                        start,
                        end: text.len(),
                        fence,
                        indent: indent.to_string(),
                        info: info.to_string(),
                        body: String::new(),
                        closed: false,
                    });
                }
            }
        }
    }
    blocks.extend(open);
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_splits_into_the_expected_top_level_blocks() {
        for case in corpus::FENCE_CORPUS {
            let blocks = fenced_blocks(case.input);
            let got: Vec<(char, &str, &str, &str, bool)> = blocks
                .iter()
                .map(|b| {
                    let indent = b.indent.as_str();
                    (
                        b.fence.ch,
                        indent,
                        b.info.as_str(),
                        b.body.as_str(),
                        b.closed,
                    )
                })
                .collect();
            let want: Vec<(char, &str, &str, &str, bool)> = case
                .blocks
                .iter()
                .map(|e| (e.fence, e.indent, e.info, e.body, e.closed))
                .collect();
            assert_eq!(got, want, "{}", case.name);
        }
    }

    #[test]
    fn offsets_cover_the_fence_lines() {
        let text = "a\n```rust x\nfn f() {}\n```\nb\n~~~\nopen\n";
        let blocks = fenced_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            &text[blocks[0].start..blocks[0].end],
            "```rust x\nfn f() {}\n```\n"
        );
        assert_eq!(blocks[0].label(), "rust");
        assert_eq!(blocks[0].fence, Fence { ch: '`', len: 3 });
        assert_eq!(&text[blocks[1].start..blocks[1].end], "~~~\nopen\n");
        assert_eq!(blocks[1].label(), "");
        assert!(!blocks[1].closed);
    }

    #[test]
    fn closing_fence_rules() {
        let fence = Fence { ch: '`', len: 4 };
        assert!(fence.closes("````"));
        assert!(fence.closes("  `````  \n"));
        assert!(fence.closes("````\r\n"));
        assert!(!fence.closes("```"));
        assert!(!fence.closes("```` x"));
        assert!(!fence.closes("~~~~"));
    }

    #[test]
    fn opening_fence_rules() {
        assert_eq!(fence_open("``"), None);
        assert_eq!(fence_open("```js`x"), None);
        assert_eq!(
            fence_open("  ~~~ a`b \n"),
            Some((Fence { ch: '~', len: 3 }, "a`b"))
        );
        assert_eq!(
            fence_open("`````tool_calls json"),
            Some((Fence { ch: '`', len: 5 }, "tool_calls json"))
        );
    }
}
