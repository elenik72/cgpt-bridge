//! Reply parsing: ChatGPT's answer back into plain text or tool calls. Calls
//! come as exactly one fenced block labelled `tool_calls` (any case, first
//! word of the info string) holding a JSON array
//! `[{"name": "...", "arguments": {...}}]`; fences follow CommonMark (three
//! or more backticks or tildes; the scanner shared with the CLI lives in
//! `cgpt_bridge_protocol::fence`). Other code blocks are plain text, unless
//! they hold calls under the wrong label. Calls are checked against the
//! request's tool schemas; what is wrong is said in words ChatGPT can act on
//! in the repair request. Small slips that leave no doubt are accepted: one
//! call without the array, `arguments` as a JSON string, `null` for an
//! optional parameter.
//!
//! ChatGPT's stream sometimes delivers a code block as JSX-like markup,
//! `<CodeBlock language="X">BODY</CodeBlock>`, instead of a fence; it is
//! read as a fenced block labelled X. It also autolinks URLs inside the
//! JSON as `[U](U)`; in argument strings such a link is read as plain `U`.
//!
//! Text without a `tool_calls` block that is still shaped like a call (see
//! [`call_shaped`]) is never run: it only earns the repair request, so that
//! a call ChatGPT wrote in another form is not lost as text. Live cases of
//! every form are kept in `tests/fixtures/replies/`.

use std::borrow::Cow;

use cgpt_bridge_protocol::fence::{fence_open, fenced_blocks, Block, Fence};
use serde_json::{Map, Value};

use crate::openai::Tool;

const LABEL: &str = "tool_calls";

#[derive(Debug, Clone, PartialEq)]
pub enum ParseOutcome {
    /// No `tool_calls` block: the final answer for the user.
    Text(String),
    /// The calls (tool name, arguments object) and the text before the block.
    Calls {
        prefix: String,
        calls: Vec<(String, Value)>,
    },
    /// A `tool_calls` block that cannot be used, and why.
    Invalid(String),
    /// No `tool_calls` block, but ChatGPT's own widget markup outside code
    /// blocks: what to ask ChatGPT instead, and the answer as text.
    Widget { problem: String, text: String },
    /// No `tool_calls` block, but something shaped like a call in another
    /// form (other keys, the OpenAI wire form, JSON without a block, a tool
    /// name followed by its arguments): what to ask ChatGPT, and the answer
    /// as text. Nothing of it is ever run.
    CallShaped { problem: String, text: String },
}

/// ChatGPT's own interactive widgets reach the page text as their source
/// markup (`{@body ...}` and `GenUI.` calls), which means nothing to the user.
/// Only lines outside fenced blocks count: code that shows such markup is an
/// answer like any other.
fn has_widget(text: &str) -> bool {
    let mut open: Option<Fence> = None;
    for line in text.lines() {
        match &open {
            Some(fence) => {
                if fence.closes(line) {
                    open = None;
                }
            }
            None => {
                if let Some((fence, _)) = fence_open(line) {
                    open = Some(fence);
                } else if line.trim_start().starts_with("{@body") || line.contains("GenUI.") {
                    return true;
                }
            }
        }
    }
    false
}

pub fn parse_reply(text: &str, tools: &[Tool]) -> ParseOutcome {
    let (text, stray_calls_tag) = codeblocks_to_fences(text);
    let text = &*text;
    // The whole answer twice over, word for word (seen live with the label
    // `tool_calls id="..."`, cause not confirmed): the calls of one copy,
    // which leave no doubt. Anything else is read as it is.
    if let Some(half) = doubled_half(text).filter(|_| text.contains(LABEL)) {
        if let calls @ ParseOutcome::Calls { .. } = parse_reply(half, tools) {
            return calls;
        }
    }
    let blocks = fenced_blocks(text);
    let ours: Vec<&Block> = blocks
        .iter()
        .filter(|b| b.label().eq_ignore_ascii_case(LABEL))
        .collect();
    let block = match ours.as_slice() {
        [] => {
            // Calls in a block with another label (or none) would be lost
            // as text: ask for the right label instead.
            if let Some(b) = blocks
                .iter()
                .find(|b| looks_like_calls(&block_body(b), tools))
            {
                return ParseOutcome::Invalid(format!(
                    "вызовы оформлены блоком «{}», нужна метка tool_calls",
                    label_name(b)
                ));
            }
            if stray_calls_tag {
                return ParseOutcome::Invalid(
                    "блок tool_calls пришёл разметкой <CodeBlock>, которую не удалось \
                     разобрать: пришли вызовы обычным блоком ```tool_calls"
                        .to_string(),
                );
            }
            // Calls written line by line in inline code, no fence at all
            // (live 05.11: `[`, `  {`, `"name": "shell",` ... each line in
            // backticks): read them as the block they stand for.
            if let Some((start, body)) = backticked_calls(text, tools) {
                return match parse_calls(&body, tools) {
                    Ok(calls) => ParseOutcome::Calls {
                        prefix: text[..start].trim().to_string(),
                        calls,
                    },
                    Err(problem) => ParseOutcome::Invalid(problem),
                };
            }
            if let Some(found) = call_shaped_in(text, &blocks, tools) {
                return match found {
                    Found::Calls(problem) => ParseOutcome::Invalid(problem),
                    Found::Shaped(problem) => ParseOutcome::CallShaped {
                        problem,
                        text: text.trim().to_string(),
                    },
                };
            }
            if has_widget(text) {
                return ParseOutcome::Widget {
                    problem: "в ответе интерактивный элемент ChatGPT (кнопки, переключатели), \
                              программа пользователя его не покажет: чтобы спросить \
                              пользователя, вызови инструмент question, если он есть в \
                              списке, иначе спроси обычным текстом"
                        .to_string(),
                    text: text.trim().to_string(),
                };
            }
            return ParseOutcome::Text(text.trim().to_string());
        }
        [block] => *block,
        many => {
            return ParseOutcome::Invalid(format!(
                "блоков tool_calls в ответе: {}, а нужен ровно один — \
                 собери все вызовы в один блок",
                many.len()
            ));
        }
    };
    if !block.closed {
        return ParseOutcome::Invalid(
            "блок tool_calls не закрыт: после JSON-массива нужна строка ```".to_string(),
        );
    }
    // The same lines in inline code, inside the block (live 05.11).
    let body = unwrap_backticked_lines(&block.body).unwrap_or_else(|| block.body.clone());
    match parse_calls(&body, tools) {
        Ok(calls) => ParseOutcome::Calls {
            prefix: text[..block.start].trim().to_string(),
            calls,
        },
        Err(problem) => ParseOutcome::Invalid(problem),
    }
}

/// A line that is all one inline-code span: `` `x` ``, maybe indented.
fn backticked_line(line: &str) -> Option<&str> {
    let t = line.trim();
    let inner = t.strip_prefix('`')?.strip_suffix('`')?;
    (!inner.contains('`')).then_some(inner)
}

/// `body` with the backticks around every line removed, when every
/// non-empty line is one inline-code span; `None` otherwise.
fn unwrap_backticked_lines(body: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut any = false;
    for line in body.lines() {
        if line.trim().is_empty() {
            out.push("");
            continue;
        }
        out.push(backticked_line(line)?);
        any = true;
    }
    any.then(|| out.join("\n"))
}

/// Outside fenced blocks, the first run of two or more lines that are each
/// one inline-code span and together hold calls: where the run starts in
/// `text`, and its lines without the backticks.
fn backticked_calls(text: &str, tools: &[Tool]) -> Option<(usize, String)> {
    let mut open: Option<Fence> = None;
    let mut run: Option<(usize, Vec<&str>)> = None;
    let mut offset = 0;
    let check = |run: &mut Option<(usize, Vec<&str>)>| -> Option<(usize, String)> {
        let (start, lines) = run.take()?;
        if lines.len() < 2 {
            return None;
        }
        let body = lines.join("\n");
        looks_like_calls(&body, tools).then_some((start, body))
    };
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some(fence) = &open {
            if fence.closes(line) {
                open = None;
            }
            continue;
        }
        if let Some((fence, _)) = fence_open(line) {
            if let Some(found) = check(&mut run) {
                return Some(found);
            }
            open = Some(fence);
            continue;
        }
        match backticked_line(line) {
            Some(inner) => run.get_or_insert_with(|| (start, Vec::new())).1.push(inner),
            None if line.trim().is_empty() => {}
            None => {
                if let Some(found) = check(&mut run) {
                    return Some(found);
                }
            }
        }
    }
    check(&mut run)
}

/// A block's body, the backticks around its lines removed when every line
/// is one inline-code span.
fn block_body(b: &Block) -> String {
    unwrap_backticked_lines(&b.body).unwrap_or_else(|| b.body.clone())
}

/// A block's label as the repair request names it.
fn label_name(b: &Block) -> &str {
    if b.label().is_empty() {
        "без метки"
    } else {
        b.label()
    }
}

/// What [`call_shaped_in`] found.
enum Found {
    /// Calls in the gateway's own form (`name` of an offered tool and
    /// `arguments`), only not in a `tool_calls` block.
    Calls(String),
    /// A call in another form.
    Shaped(String),
}

/// Keys ChatGPT has been seen to use (or may use) for a tool's name and for
/// its arguments in call-like JSON. `function` is the OpenAI wire form.
const NAME_KEYS: [&str; 4] = ["name", "tool", "tool_name", "recipient_name"];
const ARGS_KEYS: [&str; 5] = ["arguments", "args", "parameters", "input", "params"];
/// Keys of an object that wraps a list of calls.
const WRAPPER_KEYS: [&str; 3] = ["tool_calls", "calls", "tool_uses"];

/// The offered tool `name` stands for (`functions.<name>` is OpenAI's
/// recipient form).
fn offered<'a>(name: &str, tools: &'a [Tool]) -> Option<&'a Tool> {
    let name = name.strip_prefix("functions.").unwrap_or(name);
    tools.iter().find(|t| t.function.name == name)
}

/// The name of the tool a call-like JSON value calls, when it is one in a
/// form other than the gateway's own: an object (or a non-empty array of
/// objects, or an object wrapping such an array under `tool_calls`) that
/// names an offered tool under a name key and has arguments under an
/// arguments key, or `{"function": {"name": ...}}` naming an offered tool.
/// `{"name": X, "arguments": {...}}` counts for any X: an unknown name is
/// still a call (one the repair names as unknown).
fn shaped_call(value: &Value, tools: &[Tool]) -> Option<String> {
    match value {
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            items.iter().find_map(|i| shaped_call(i, tools))
        }
        Value::Object(obj) => {
            if let Some(inner) = WRAPPER_KEYS.iter().find_map(|k| obj.get(*k)) {
                return shaped_call(inner, tools);
            }
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .filter(|n| offered(n, tools).is_some())
            {
                return Some(name.to_string());
            }
            if let (Some(name), Some(args)) = (
                obj.get("name").and_then(Value::as_str),
                obj.get("arguments"),
            ) {
                if args.is_object() || args.is_string() {
                    return Some(name.to_string());
                }
            }
            let name = NAME_KEYS
                .iter()
                .chain(["function"].iter())
                .filter_map(|k| obj.get(*k).and_then(Value::as_str))
                .find(|n| offered(n, tools).is_some())?;
            // A tool's definition (`description`, `parameters` as a JSON
            // schema) names it too, and is no call.
            let schema_like = |a: &Value| {
                a.get("properties").is_some() || a.get("type") == Some(&Value::from("object"))
            };
            let described = obj.contains_key("description");
            ARGS_KEYS
                .iter()
                .filter_map(|k| obj.get(*k))
                .any(|a| a.is_object() && !schema_like(a) && !described)
                .then(|| name.to_string())
        }
        _ => None,
    }
}

/// Whether `value` is an arguments object for `tool` and nothing else: not
/// empty, every key a parameter of the tool, every required one there, and
/// every type right.
fn fits_tool(value: &Value, tool: &Tool) -> bool {
    let Some(args) = value.as_object().filter(|a| !a.is_empty()) else {
        return false;
    };
    let schema = &tool.function.parameters;
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return false;
    };
    args.keys().all(|k| props.contains_key(k)) && argument_problems(args, schema).is_empty()
}

/// The offered tools named as a whole word in `s` (not inside a longer word
/// or a quoted JSON string).
fn named_tools<'a>(s: &str, tools: &'a [Tool]) -> Vec<&'a Tool> {
    tools
        .iter()
        .filter(|t| word_at(s, &t.function.name).next().is_some())
        .collect()
}

/// Byte offsets just past every whole-word occurrence of `word` in `s`.
fn word_at<'a>(s: &'a str, word: &'a str) -> impl Iterator<Item = usize> + 'a {
    let part = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '"' | '.' | '/');
    s.match_indices(word).filter_map(move |(i, _)| {
        let end = i + word.len();
        let before = s[..i].chars().next_back().is_none_or(|c| !part(c));
        let after = s[end..].chars().next().is_none_or(|c| !part(c));
        (before && after).then_some(end)
    })
}

/// The first JSON value at the start of `s` (after whitespace), whatever
/// follows it.
fn json_prefix(s: &str) -> Option<Value> {
    serde_json::Deserializer::from_str(s)
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// Something call-shaped in an answer without a `tool_calls` block:
/// - a fenced block with any other label (or none), closed or not, holding
///   call-like JSON;
/// - call-like JSON outside fences, starting a line (maybe as one inline-code
///   span);
/// - an offered tool's name followed on its line (after `(`, `:`, spaces or
///   backticks) by a JSON object that is exactly an arguments object of that
///   tool, or a line naming a tool right before such an object (fenced or
///   not).
///
/// Prose that only names a tool, JSON that is not call-like and code that is
/// not JSON stay text.
fn call_shaped_in(text: &str, blocks: &[Block], tools: &[Tool]) -> Option<Found> {
    let shaped = |where_: &str, name: &str| {
        let unknown = match offered(name, tools) {
            Some(_) => String::new(),
            None => {
                let known: Vec<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();
                format!(
                    "; инструмента «{}» нет, доступны: {}",
                    name,
                    known.join(", ")
                )
            }
        };
        Found::Shaped(format!(
            "{} похоже на вызов инструмента, но это не блок tool_calls: вызовы пишутся \
             блоком ```tool_calls с массивом [{{\"name\": \"...\", \"arguments\": {{...}}}}]{}",
            where_, unknown
        ))
    };
    // Fenced blocks of another label.
    for b in blocks {
        let body = block_body(b);
        if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
            if let Some(name) = shaped_call(&value, tools) {
                return Some(shaped(&format!("блок «{}»", label_name(b)), &name));
            }
        }
    }
    // Lines outside fences: bare JSON, and a tool name with its arguments.
    let mut prev_line = "";
    let mut offset = 0;
    let mut blocks_left = blocks.iter().peekable();
    while offset < text.len() {
        if let Some(b) = blocks_left.peek() {
            if offset >= b.start {
                // A block right after a line naming a tool: its body may be
                // that tool's arguments.
                if let Ok(value) = serde_json::from_str::<Value>(block_body(b).trim()) {
                    if let Some(tool) = named_tools(prev_line, tools)
                        .into_iter()
                        .find(|t| fits_tool(&value, t))
                    {
                        return Some(shaped(
                            &format!(
                                "аргументы «{}» в блоке «{}»",
                                tool.function.name,
                                label_name(b)
                            ),
                            &tool.function.name,
                        ));
                    }
                }
                offset = b.end.max(offset + 1);
                prev_line = "";
                blocks_left.next();
                continue;
            }
        }
        let rest = &text[offset..];
        let line_end = rest.find('\n').map_or(rest.len(), |i| i + 1);
        let line = rest[..line_end].trim_end_matches(['\n', '\r']);
        let inner = backticked_line(line).unwrap_or(line).trim_start();
        if inner.starts_with('{') || inner.starts_with('[') {
            // From the brace on, the value may run over several lines.
            let from = if backticked_line(line).is_some() {
                inner.to_string()
            } else {
                rest[rest.find(['{', '[']).unwrap_or(0)..].to_string()
            };
            if let Some(value) = json_prefix(&from) {
                if is_calls(&value, tools) {
                    return Some(Found::Calls(
                        "вызовы написаны JSON без блока: нужен блок ```tool_calls".to_string(),
                    ));
                }
                if let Some(name) = shaped_call(&value, tools) {
                    return Some(shaped("JSON без блока", &name));
                }
                if let Some(tool) = named_tools(prev_line, tools)
                    .into_iter()
                    .find(|t| fits_tool(&value, t))
                {
                    let name = &tool.function.name;
                    return Some(shaped(&format!("аргументы «{}» без блока", name), name));
                }
            }
        }
        for tool in tools {
            let name = &tool.function.name;
            for end in word_at(line, name) {
                let after = line[end..].trim_start_matches(|c: char| {
                    c.is_whitespace() || matches!(c, '`' | '(' | ':')
                });
                if !after.starts_with('{') {
                    continue;
                }
                // The object may run past this line.
                let at = offset + (line.len() - after.len());
                if json_prefix(&text[at..]).is_some_and(|v| fits_tool(&v, tool)) {
                    return Some(shaped(&format!("«{} {{...}}» текстом", name), name));
                }
            }
        }
        if !line.trim().is_empty() {
            prev_line = line;
        }
        offset += line_end;
    }
    None
}

/// Most whitespace between the two copies of a doubled answer.
const MAX_SEAM_BYTES: usize = 256;

/// The first copy of an answer that is the same text twice over, when it
/// is; `None` otherwise. The copies must be byte for byte the same; only
/// whitespace at the ends and at the seam between them (none at all too:
/// right after a closing fence) is not compared. Whitespace inside a copy
/// counts: `rm -r ./build /tmp/x` and `rm -r ./build/tmp/x` are two calls.
fn doubled_half(text: &str) -> Option<&str> {
    let t = text.trim();
    (0..=MAX_SEAM_BYTES.min(t.len()))
        .filter(|seam| (t.len() - seam) % 2 == 0)
        .find_map(|seam| {
            let half = (t.len() - seam) / 2;
            let (first, rest) = (t.get(..half)?, t.get(half..)?);
            let (gap, second) = (rest.get(..seam)?, rest.get(seam..)?);
            (half > 0 && gap.chars().all(char::is_whitespace) && first == second).then_some(first)
        })
}

/// Whether the answer is the same text twice over (see [`doubled_half`]),
/// for the gateway's log of unusual answers.
pub fn is_doubled(text: &str) -> bool {
    let (text, _) = codeblocks_to_fences(text);
    doubled_half(&text).is_some()
}

/// Whether text handed out as the final answer still mentions calls: the
/// block label, an `"arguments"` key, or an offered tool's name as a JSON
/// string next to a `"name"` key. Weaker than [`call_shaped_in`] (no repair
/// follows); only for the gateway's log of unusual answers.
pub fn mentions_calls(text: &str, tools: &[Tool]) -> bool {
    let lower = text.to_lowercase();
    lower.contains(LABEL)
        || text.contains("\"arguments\"")
        || (text.contains("\"name\"")
            && tools
                .iter()
                .any(|t| text.contains(&format!("\"{}\"", t.function.name))))
}

/// The one automatic follow-up after an `Invalid` reply.
pub fn repair_request(problem: &str) -> String {
    format!(
        "Не получилось разобрать вызовы инструментов в твоём ответе:\n{}\n\n\
         Повтори ответ в правильном формате: ровно один блок ```tool_calls с JSON-массивом \
         [{{\"name\": \"...\", \"arguments\": {{...}}}}], имена инструментов и параметров — из \
         списка инструментов. Если вызывать инструменты не нужно, ответь текстом без блока.",
        problem
    )
}

const CODEBLOCK_OPEN: &str = "<CodeBlock";
const CODEBLOCK_CLOSE: &str = "</CodeBlock>";

/// The text with every `<CodeBlock language="X">BODY</CodeBlock>` outside
/// fenced blocks rewritten as a fenced block labelled X, entities in BODY
/// decoded. Markup that cannot be paired stays as it is; the flag says
/// whether such a leftover opening tag outside fences is labelled
/// tool_calls (calls that would otherwise be lost as text).
fn codeblocks_to_fences(text: &str) -> (Cow<'_, str>, bool) {
    if !text.contains(CODEBLOCK_OPEN) {
        return (Cow::Borrowed(text), false);
    }
    let mut stray_calls_tag = false;
    let mut out = String::with_capacity(text.len());
    let mut open: Option<Fence> = None;
    let mut pos = 0;
    while pos < text.len() {
        let rest = &text[pos..];
        let line = &rest[..rest.find('\n').map_or(rest.len(), |i| i + 1)];
        if let Some(fence) = &open {
            if fence.closes(line) {
                open = None;
            }
        } else if let Some((fence, _)) = fence_open(line) {
            open = Some(fence);
        } else if let Some((at, (label, body, len))) = line
            .match_indices(CODEBLOCK_OPEN)
            .find_map(|(at, _)| Some((at, codeblock_at(&rest[at..])?)))
        {
            stray_calls_tag |= has_calls_tag(&line[..at]);
            out.push_str(&line[..at]);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            let fence = "`".repeat(fence_len_for(&body));
            out.push_str(&format!("{}{}\n{}\n{}\n", fence, label, body, fence));
            pos += at + len;
            // The newline right after the closing tag is the fence's own.
            if text[pos..].starts_with('\n') {
                pos += 1;
            }
            continue;
        } else {
            stray_calls_tag |= has_calls_tag(line);
        }
        out.push_str(line);
        pos += line.len();
    }
    (Cow::Owned(out), stray_calls_tag)
}

/// An opening CodeBlock tag labelled tool_calls somewhere in `s`.
fn has_calls_tag(s: &str) -> bool {
    s.match_indices(CODEBLOCK_OPEN).any(|(at, _)| {
        let tag = &s[at..];
        opens_codeblock(tag)
            && tag_end(&tag[CODEBLOCK_OPEN.len()..])
                .and_then(|end| attribute(&tag[CODEBLOCK_OPEN.len()..][..end], "language"))
                .is_some_and(|l| {
                    l.split_whitespace()
                        .next()
                        .is_some_and(|w| w.eq_ignore_ascii_case(LABEL))
                })
    })
}

/// The label, body and byte length of a CodeBlock element at the start of
/// `s`. The opening tag needs a `language` attribute, and the closing tag
/// belongs to the nearest opening tag before it: a `<CodeBlock ...>` that
/// is only mentioned in prose never swallows a real block after it. The
/// body is unescaped only when it looks escaped.
fn codeblock_at(s: &str) -> Option<(String, String, usize)> {
    if !opens_codeblock(s) {
        return None;
    }
    let after = &s[CODEBLOCK_OPEN.len()..];
    let tag_end = tag_end(after)?;
    let label = attribute(&after[..tag_end], "language")?;
    let body_start = CODEBLOCK_OPEN.len() + tag_end + 1;
    let body_len = s[body_start..].find(CODEBLOCK_CLOSE)?;
    let body = &s[body_start..body_start + body_len];
    if body
        .match_indices(CODEBLOCK_OPEN)
        .any(|(i, _)| opens_codeblock(&body[i..]))
    {
        return None;
    }
    let body = body.strip_prefix('\n').unwrap_or(body);
    let body = body.strip_suffix('\n').unwrap_or(body);
    Some((
        label.split_whitespace().next().unwrap_or("").to_string(),
        unescape(body).into_owned(),
        body_start + body_len + CODEBLOCK_CLOSE.len(),
    ))
}

/// `s` starts with a CodeBlock opening tag (not `<CodeBlockX`).
fn opens_codeblock(s: &str) -> bool {
    s.strip_prefix(CODEBLOCK_OPEN)
        .is_some_and(|after| after.starts_with(|c: char| c.is_whitespace() || c == '>'))
}

/// Offset of the `>` that ends an opening tag, skipping quoted values.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            (None, _) => {}
        }
    }
    None
}

/// The value of `name="..."` or `name='...'` among a tag's attributes.
fn attribute(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(i) = rest.find(name) {
        let preceded = rest[..i]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let after = rest[i + name.len()..].trim_start();
        if let (true, Some(value)) = (preceded, after.strip_prefix('=')) {
            let value = value.trim_start();
            let quote = value.chars().next().filter(|&c| c == '"' || c == '\'')?;
            let inner = &value[1..];
            return Some(unescape(&inner[..inner.find(quote)?]).into_owned());
        }
        rest = &rest[i + name.len()..];
    }
    None
}

/// `s` with HTML entities decoded, if it looks escaped: no raw `<`, and
/// every `&` starts a known entity (`&lt; &gt; &amp; &quot; &apos;` or a
/// numeric one). Otherwise it is raw text (as seen live) and kept as it is,
/// so a literal `&lt;` in file content survives.
fn unescape(s: &str) -> Cow<'_, str> {
    if s.contains('<') || !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let Some((c, len)) = entity(&rest[i..]) else {
            return Cow::Borrowed(s);
        };
        out.push(c);
        rest = &rest[i + len..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The character and byte length of the entity at the start of `s`.
fn entity(s: &str) -> Option<(char, usize)> {
    let end = s.find(';')?;
    let name = &s[1..end];
    let c = match name {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some((c, end + 1))
}

/// A backtick fence longer than any backtick run that starts a body line.
fn fence_len_for(body: &str) -> usize {
    body.lines()
        .map(|l| l.trim_start().chars().take_while(|&c| c == '`').count() + 1)
        .max()
        .unwrap_or(0)
        .max(3)
}

/// A block body that is a call (with `arguments`) or a non-empty array of
/// calls, every one named and at least one naming a known tool.
fn looks_like_calls(body: &str, tools: &[Tool]) -> bool {
    serde_json::from_str::<Value>(body.trim()).is_ok_and(|v| is_calls(&v, tools))
}

/// [`looks_like_calls`] for a parsed value.
fn is_calls(value: &Value, tools: &[Tool]) -> bool {
    let known = |item: &Value| {
        item.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| tools.iter().any(|t| t.function.name == n))
    };
    match value {
        Value::Object(obj) => obj.contains_key("arguments") && known(value),
        Value::Array(items) => {
            !items.is_empty()
                && items
                    .iter()
                    .all(|i| i.get("name").and_then(Value::as_str).is_some())
                && items.iter().any(known)
        }
        _ => false,
    }
}

fn parse_calls(body: &str, tools: &[Tool]) -> Result<Vec<(String, Value)>, String> {
    let value: Value = serde_json::from_str(body.trim())
        .map_err(|e| format!("содержимое блока tool_calls — не корректный JSON ({})", e))?;
    let items = match value {
        Value::Array(items) => items,
        // One call without the array around it.
        Value::Object(_) => vec![value],
        other => {
            return Err(format!(
                "в блоке tool_calls должен быть JSON-массив вызовов \
                 [{{\"name\": \"...\", \"arguments\": {{...}}}}], а там {}",
                kind(&other)
            ))
        }
    };
    if items.is_empty() {
        return Err("блок tool_calls пуст: в массиве нет ни одного вызова".to_string());
    }
    let mut calls = Vec::new();
    let mut problems = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match check_call(item, tools) {
            Ok(call) => calls.push(call),
            Err(found) => {
                problems.extend(found.into_iter().map(|p| format!("вызов {}{}", i + 1, p)))
            }
        }
    }
    if problems.is_empty() {
        Ok(calls)
    } else {
        Err(problems.join("\n"))
    }
}

/// One call; each problem starts with ": " or " (<name>): " to follow
/// "вызов N".
fn check_call(item: &Value, tools: &[Tool]) -> Result<(String, Value), Vec<String>> {
    let Some(obj) = item.as_object() else {
        return Err(vec![format!(
            ": ожидался объект {{\"name\": \"...\", \"arguments\": {{...}}}}, а пришло {}",
            kind(item)
        )]);
    };
    let Some(name) = obj.get("name").and_then(Value::as_str) else {
        return Err(vec![
            ": нет строкового поля \"name\" с именем инструмента".to_string()
        ]);
    };
    let Some(tool) = tools.iter().find(|t| t.function.name == name) else {
        let known: Vec<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();
        return Err(vec![format!(
            ": инструмента «{}» нет; доступны: {}",
            name,
            known.join(", ")
        )]);
    };
    let parsed_string = match obj.get("arguments") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
        _ => None,
    };
    let args = match parsed_string.as_ref().or(obj.get("arguments")) {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(args)) => {
            let mut args = args.clone();
            drop_ignored_keys_in(&mut args, &tool.function.parameters);
            args.values_mut().for_each(unwrap_autolinks);
            args
        }
        Some(other) => {
            return Err(vec![format!(
                " ({}): поле \"arguments\" должно быть JSON-объектом, а пришло {}",
                name,
                kind(other)
            )])
        }
    };
    let problems: Vec<String> = argument_problems(&args, &tool.function.parameters)
        .into_iter()
        .map(|p| format!(" ({}): {}", name, p))
        .collect();
    if problems.is_empty() {
        Ok((name.to_string(), Value::Object(args)))
    } else {
        Err(problems)
    }
}

/// Keys OpenCode would ignore are dropped, at every depth the schema
/// describes: `null` for an optional parameter (or nested field) that does
/// not allow it means "not given", and a key an object with
/// `additionalProperties: false` does not know is dropped too. OpenCode's
/// schemas carry that as a serializer default; that OpenCode itself ignores
/// such keys is inferred (Effect Schema drops excess keys by default, and
/// live run 3 had no rejected calls), not observed.
fn drop_ignored_keys_in(map: &mut Map<String, Value>, schema: &Value) {
    let required = required_keys(schema);
    let props = schema.get("properties").and_then(Value::as_object);
    let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
    map.retain(|name, v| {
        let unknown = closed && !props.is_some_and(|p| p.contains_key(name.as_str()));
        let stray = v.is_null()
            && !required.contains(&name.as_str())
            && props
                .and_then(|p| p.get(name.as_str()))
                .and_then(declared_types)
                .is_some_and(|types| !types.contains(&"null"));
        !stray && !unknown
    });
    for (name, v) in map.iter_mut() {
        if let Some(field) = props.and_then(|p| p.get(name.as_str())) {
            drop_ignored_keys(v, field);
        }
    }
}

fn drop_ignored_keys(value: &mut Value, schema: &Value) {
    match value {
        Value::Object(map) => drop_ignored_keys_in(map, schema),
        Value::Array(items) => {
            if let Some(item) = schema.get("items").filter(|i| i.is_object()) {
                items.iter_mut().for_each(|v| drop_ignored_keys(v, item));
            }
        }
        _ => {}
    }
}

/// ChatGPT renders a URL in a JSON string as a markdown link to itself,
/// `[U](U)`; every such link in the argument strings becomes `U`.
fn unwrap_autolinks(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Cow::Owned(plain) = unwrap_autolinks_in(s) {
                *s = plain;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(unwrap_autolinks),
        Value::Object(map) => map.values_mut().for_each(unwrap_autolinks),
        _ => {}
    }
}

/// `[U](U)` with the same http(s) URL as text and target becomes `U`;
/// every other bracket stays.
fn unwrap_autolinks_in(s: &str) -> Cow<'_, str> {
    if !s.contains("](http") {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let link = after.find(']').and_then(|close| {
            let url = &after[..close];
            let is_url = url.starts_with("https://") || url.starts_with("http://");
            let tail = after[close + 1..].strip_prefix('(')?.strip_prefix(url)?;
            is_url.then_some((url, tail.strip_prefix(')')?))
        });
        match link {
            Some((url, tail)) => {
                out.push_str(url);
                rest = tail;
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn required_keys(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Missing required parameters, type mismatches and arrays shorter than
/// `minItems`, at every depth the schema describes (object `properties`,
/// array `items`). Below the top level a problem starts with the JSON path
/// of the field.
fn argument_problems(args: &Map<String, Value>, schema: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    for name in required_keys(schema) {
        if !args.contains_key(name) {
            problems.push(format!("нет обязательного параметра «{}»", name));
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    for (name, value) in args {
        let Some(field) = props.and_then(|p| p.get(name)) else {
            continue;
        };
        if let Some(types) = declared_types(field) {
            if !types.iter().any(|t| has_type(value, t)) {
                problems.push(format!(
                    "параметр «{}» должен быть {}, а пришло {}",
                    name,
                    types.join("|"),
                    kind(value)
                ));
                continue;
            }
        }
        nested_problems(value, field, name, &mut problems);
    }
    problems
}

/// What is wrong inside `value` (already of a type its schema allows).
fn nested_problems(value: &Value, schema: &Value, path: &str, problems: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for name in required_keys(schema) {
                if !map.contains_key(name) {
                    problems.push(format!("{}.{}: нет обязательного поля", path, name));
                }
            }
            let props = schema.get("properties").and_then(Value::as_object);
            for (name, v) in map {
                if let Some(field) = props.and_then(|p| p.get(name)) {
                    field_problems(v, field, &format!("{}.{}", path, name), problems);
                }
            }
        }
        Value::Array(items) => {
            let min = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            if (items.len() as u64) < min {
                problems.push(format!(
                    "{}: нужно не меньше {} элементов, а пришло {}",
                    path,
                    min,
                    items.len()
                ));
            }
            if let Some(item) = schema.get("items").filter(|i| i.is_object()) {
                for (i, v) in items.iter().enumerate() {
                    field_problems(v, item, &format!("{}[{}]", path, i), problems);
                }
            }
        }
        _ => {}
    }
}

fn field_problems(value: &Value, schema: &Value, path: &str, problems: &mut Vec<String>) {
    if let Some(types) = declared_types(schema) {
        if !types.iter().any(|t| has_type(value, t)) {
            problems.push(format!(
                "{}: должно быть {}, а пришло {}",
                path,
                types.join("|"),
                kind(value)
            ));
            return;
        }
    }
    nested_problems(value, schema, path, problems);
}

fn declared_types(schema: &Value) -> Option<Vec<&str>> {
    match schema.get("type")? {
        Value::String(t) => Some(vec![t.as_str()]),
        Value::Array(ts) => Some(ts.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

fn has_type(value: &Value, t: &str) -> bool {
    match t {
        "string" => value.is_string(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // A type the gateway does not know is not held against the call.
        _ => true,
    }
}

/// The JSON-schema name of a value's type.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::ToolFunction;
    use serde_json::json;

    fn tools() -> Vec<Tool> {
        let tool = |name: &str, parameters: Value| Tool {
            kind: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: None,
                parameters,
            },
        };
        vec![
            tool(
                "read",
                json!({"type": "object", "required": ["path"], "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "verbose": {"type": ["boolean", "null"]}
                }}),
            ),
            tool(
                "glob",
                json!({"type": "object", "required": ["pattern"], "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"}
                }}),
            ),
            tool(
                "patch",
                json!({"type": "object", "required": ["patchText"], "properties": {
                    "patchText": {"type": "string"}
                }}),
            ),
        ]
    }

    fn invalid(text: &str) -> String {
        match parse_reply(text, &tools()) {
            ParseOutcome::Invalid(problem) => {
                assert!(!problem.is_empty());
                problem
            }
            other => panic!("expected Invalid, got {:?}", other),
        }
    }

    // Live 05.11 (Windows): calls written line by line in inline code.
    const BACKTICKED: &str = "`[`\n`  {`\n`    \"name\": \"read\",`\n`    \"arguments\": {`\n`      \"path\": \"C:\\\\scanner opencode\\\\a.cs\",`\n`      \"offset\": 1`\n`    }`\n`  }`\n`]`";

    #[test]
    fn calls_line_by_line_in_inline_code_without_a_fence_are_calls() {
        let text = format!("Сначала прочитаю файл.\n{}\n", BACKTICKED);
        match parse_reply(&text, &tools()) {
            ParseOutcome::Calls { prefix, calls } => {
                assert_eq!(prefix, "Сначала прочитаю файл.");
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].0, "read");
                assert_eq!(calls[0].1["path"], "C:\\scanner opencode\\a.cs");
            }
            other => panic!("expected Calls, got {:?}", other),
        }
    }

    #[test]
    fn calls_line_by_line_in_inline_code_inside_the_block_are_calls() {
        let text = format!("```tool_calls\n{}\n```", BACKTICKED);
        match parse_reply(&text, &tools()) {
            ParseOutcome::Calls { calls, .. } => assert_eq!(calls[0].0, "read"),
            other => panic!("expected Calls, got {:?}", other),
        }
    }

    #[test]
    fn inline_code_lines_that_are_not_calls_stay_text() {
        let text = "Команды:\n`git status`\n`git log --oneline`\n";
        assert_eq!(
            parse_reply(text, &tools()),
            ParseOutcome::Text(text.trim().to_string())
        );
    }

    #[test]
    fn chatgpt_widget_markup_asks_for_the_question_tool() {
        // Seen live on 26.10.2026: a radio group instead of a question call.
        let answer = "Какую операцию добавить?\n\n{@body const [op,setOp] = DIL.useState(\"division\")}\n<box gap={3}>\n  <button onClick={()=>GenUI.issueNewTurn(op)}>Подтвердить</button>\n</box>";
        match parse_reply(answer, &tools()) {
            ParseOutcome::Widget { problem, text } => {
                assert!(problem.contains("question"), "{}", problem);
                assert_eq!(text, answer);
            }
            other => panic!("expected Widget, got {:?}", other),
        }
        // Plain prose about the same things stays text.
        assert!(matches!(
            parse_reply("Добавить деление, степень или остаток?", &tools()),
            ParseOutcome::Text(_)
        ));
    }

    #[test]
    fn widget_markup_inside_a_code_block_is_plain_text() {
        // Code that mentions the markup, shown as code, is an answer.
        let answer = "Так выглядит разметка виджета:\n\n```jsx\n{@body const [op,setOp] = DIL.useState(1)}\n<button onClick={()=>GenUI.issueNewTurn(op)}>Да</button>\n```\n\nЕё видно только в ChatGPT.";
        assert_eq!(
            parse_reply(answer, &tools()),
            ParseOutcome::Text(answer.into())
        );
    }

    #[test]
    fn parse_text_without_block() {
        assert_eq!(
            parse_reply("Готово, файл исправлен. 👍\n", &tools()),
            ParseOutcome::Text("Готово, файл исправлен. 👍".into())
        );
    }

    #[test]
    fn parse_single_block_multiple_calls_with_prefix_text() {
        let text = "Сначала посмотрю файлы.\n\n```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"а.txt\", \"offset\": 10}},\n {\"name\": \"glob\", \"arguments\": {\"pattern\": \"**/*.rs\"}}]\n```\n";
        assert_eq!(
            parse_reply(text, &tools()),
            ParseOutcome::Calls {
                prefix: "Сначала посмотрю файлы.".into(),
                calls: vec![
                    ("read".into(), json!({"path": "а.txt", "offset": 10})),
                    ("glob".into(), json!({"pattern": "**/*.rs"})),
                ],
            }
        );
        // No text before the block; null fits a nullable parameter.
        let bare = "```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"b\", \"verbose\": null}}]\n```";
        assert_eq!(
            parse_reply(bare, &tools()),
            ParseOutcome::Calls {
                prefix: String::new(),
                calls: vec![("read".into(), json!({"path": "b", "verbose": null}))],
            }
        );
    }

    #[test]
    fn parse_ignores_other_code_blocks() {
        let text = "Пример формата:\n```json\n[{\"name\": \"nope\", \"arguments\": {}}]\n```\nИ ещё:\n```\nplain\n```\nА теперь вызов:\n```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"b.txt\"}}]\n```\nЖду результат.";
        match parse_reply(text, &tools()) {
            ParseOutcome::Calls { prefix, calls } => {
                assert!(prefix.starts_with("Пример формата:\n```json"), "{}", prefix);
                assert!(prefix.ends_with("А теперь вызов:"), "{}", prefix);
                assert_eq!(calls, vec![("read".into(), json!({"path": "b.txt"}))]);
            }
            other => panic!("{:?}", other),
        }
        let only_json = "Вот пример:\n```json\n{\"a\": 1}\n```";
        assert_eq!(
            parse_reply(only_json, &tools()),
            ParseOutcome::Text(only_json.into())
        );
    }

    #[test]
    fn parse_two_tool_blocks_invalid() {
        let block = "```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]\n```";
        let problem = invalid(&format!("{}\nи ещё\n{}", block, block));
        assert!(problem.contains("ровно один"), "{}", problem);
    }

    fn calls(text: &str) -> Vec<(String, Value)> {
        match parse_reply(text, &tools()) {
            ParseOutcome::Calls { calls, .. } => calls,
            other => panic!("expected Calls, got {:?}", other),
        }
    }

    #[test]
    fn parse_unclosed_block_or_bad_json_invalid() {
        let problem =
            invalid("```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]\n");
        assert!(problem.contains("не закрыт"), "{}", problem);
        let problem = invalid("```tool_calls\n[{\"name\": \"read\", arguments: {}}]\n```");
        assert!(problem.contains("JSON"), "{}", problem);
        let problem = invalid(
            "```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}},]\n```",
        );
        assert!(problem.contains("JSON"), "trailing comma: {}", problem);
        let problem = invalid(
            "```tool_calls\n// read it\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]\n```",
        );
        assert!(problem.contains("JSON"), "comment: {}", problem);
        let problem = invalid("```tool_calls\n\"read\"\n```");
        assert!(problem.contains("массив"), "{}", problem);
        let problem = invalid("```tool_calls\n[]\n```");
        assert!(problem.contains("пуст"), "{}", problem);
    }

    #[test]
    fn parse_lenient_shapes_without_doubt() {
        // One call without the array.
        assert_eq!(
            calls("```tool_calls\n{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}\n```"),
            vec![("read".into(), json!({"path": "a"}))]
        );
        // `arguments` as a JSON string holding an object (the OpenAI wire form).
        assert_eq!(
            calls("```tool_calls\n[{\"name\": \"read\", \"arguments\": \"{\\\"path\\\": \\\"a\\\"}\"}]\n```"),
            vec![("read".into(), json!({"path": "a"}))]
        );
        let problem =
            invalid("```tool_calls\n[{\"name\": \"read\", \"arguments\": \"path=a\"}]\n```");
        assert!(problem.contains("\"arguments\""), "{}", problem);
        // null for an optional parameter that does not allow it: dropped.
        assert_eq!(
            calls("```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\", \"offset\": null}}]\n```"),
            vec![("read".into(), json!({"path": "a"}))]
        );
        // ...but not for a required one.
        let problem =
            invalid("```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": null}}]\n```");
        assert!(
            problem.contains("«path» должен быть string, а пришло null"),
            "{}",
            problem
        );
    }

    #[test]
    fn parse_fences_of_four_backticks_and_tildes() {
        let call = "[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]";
        let want = vec![("read".to_string(), json!({"path": "a"}))];
        assert_eq!(calls(&format!("````tool_calls\n{}\n````", call)), want);
        assert_eq!(calls(&format!("~~~tool_calls\n{}\n~~~", call)), want);
        // A longer closing run closes; a shorter one does not.
        assert_eq!(calls(&format!("```tool_calls\n{}\n`````", call)), want);
        let problem = invalid(&format!("````tool_calls\n{}\n```\n", call));
        assert!(problem.contains("не закрыт"), "{}", problem);

        // An example inside a four-backtick block is not a call.
        let example = format!(
            "Формат такой:\n````markdown\n```tool_calls\n{}\n```\n````\nВот и всё.",
            call
        );
        assert_eq!(
            parse_reply(&example, &tools()),
            ParseOutcome::Text(example.clone())
        );
    }

    #[test]
    fn parse_patch_with_code_fence_inside_json_string() {
        let text = "Правлю README.\n```tool_calls\n[{\"name\": \"patch\", \"arguments\": {\"patchText\": \"*** Begin Patch\\n*** Update File: README.md\\n+```rust\\n+fn main() {}\\n+```\\n*** End Patch\"}}]\n```";
        assert_eq!(
            parse_reply(text, &tools()),
            ParseOutcome::Calls {
                prefix: "Правлю README.".into(),
                calls: vec![(
                    "patch".into(),
                    json!({"patchText": "*** Begin Patch\n*** Update File: README.md\n+```rust\n+fn main() {}\n+```\n*** End Patch"})
                )],
            }
        );
    }

    #[test]
    fn parse_call_shaped_block_with_wrong_label_invalid() {
        let call = "[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]";
        let problem = invalid(&format!("Вот:\n```json\n{}\n```", call));
        assert_eq!(
            problem,
            "вызовы оформлены блоком «json», нужна метка tool_calls"
        );
        let problem = invalid(&format!("```\n{}\n```", call));
        assert_eq!(
            problem,
            "вызовы оформлены блоком «без метки», нужна метка tool_calls"
        );
        let problem =
            invalid("```json\n{\"name\": \"glob\", \"arguments\": {\"pattern\": \"*\"}}\n```");
        assert!(problem.contains("«json»"), "{}", problem);
        // Ordinary JSON stays text.
        for text in [
            "```json\n{\"a\": 1}\n```",
            "```json\n[{\"name\": \"Alice\", \"age\": 3}]\n```",
            "```json\n{\"name\": \"read\"}\n```",
        ] {
            assert_eq!(parse_reply(text, &tools()), ParseOutcome::Text(text.into()));
        }
    }

    #[test]
    fn parse_info_string_first_word_is_the_label() {
        assert_eq!(
            calls("```tool_calls json\n[{\"name\": \"glob\", \"arguments\": {\"pattern\": \"*\"}}]\n```"),
            vec![("glob".into(), json!({"pattern": "*"}))]
        );
        let not_ours = "```tool_callsx\n[1]\n```";
        assert_eq!(
            parse_reply(not_ours, &tools()),
            ParseOutcome::Text(not_ours.into())
        );
    }

    #[test]
    fn parse_unknown_tool_invalid() {
        let problem =
            invalid("```tool_calls\n[{\"name\": \"delete_everything\", \"arguments\": {}}]\n```");
        assert!(problem.contains("«delete_everything»"), "{}", problem);
        assert!(
            problem.contains("доступны: read, glob, patch"),
            "{}",
            problem
        );
        let problem = invalid("```tool_calls\n[{\"arguments\": {}}]\n```");
        assert!(problem.contains("\"name\""), "{}", problem);
    }

    #[test]
    fn parse_missing_required_param_invalid() {
        let problem = invalid(
            "```tool_calls\n[{\"name\": \"glob\", \"arguments\": {\"pattern\": \"*\"}}, {\"name\": \"read\", \"arguments\": {\"offset\": 1}}]\n```",
        );
        assert!(problem.contains("вызов 2 (read)"), "{}", problem);
        assert!(problem.contains("«path»"), "{}", problem);
        assert!(!problem.contains("вызов 1"), "{}", problem);
    }

    #[test]
    fn parse_wrong_top_level_type_invalid() {
        let problem = invalid(
            "```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": 5, \"offset\": \"10\", \"verbose\": 1}}]\n```",
        );
        assert!(
            problem.contains("«path» должен быть string, а пришло integer"),
            "{}",
            problem
        );
        assert!(
            problem.contains("«offset» должен быть integer, а пришло string"),
            "{}",
            problem
        );
        assert!(
            problem.contains("«verbose» должен быть boolean|null"),
            "{}",
            problem
        );
        let problem = invalid("```tool_calls\n[{\"name\": \"read\", \"arguments\": [\"a\"]}]\n```");
        assert!(problem.contains("\"arguments\""), "{}", problem);
        let problem = invalid("```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\", \"offset\": 1.5}}]\n```");
        assert!(
            problem.contains("«offset» должен быть integer"),
            "{}",
            problem
        );
    }

    #[test]
    fn parse_block_label_case_insensitive() {
        for label in ["TOOL_CALLS", "Tool_Calls", " tool_calls "] {
            let text = format!(
                "```{}\n[{{\"name\": \"glob\", \"arguments\": {{\"pattern\": \"*\"}}}}]\n```",
                label
            );
            assert_eq!(
                parse_reply(&text, &tools()),
                ParseOutcome::Calls {
                    prefix: String::new(),
                    calls: vec![("glob".into(), json!({"pattern": "*"}))],
                },
                "label {:?}",
                label
            );
        }
    }

    /// The whole answer seen live on 26.10.2026: the block came as JSX-like
    /// markup, the URL autolinked as a markdown link.
    const CODEBLOCK_SAMPLE: &str = r#"<CodeBlock language="tool_calls">[{"name":"shell","arguments":{"command":"curl -fsSL [https://example.invalid/install.sh](https://example.invalid/install.sh) | sh"}}]</CodeBlock>"#;

    fn with_shell() -> Vec<Tool> {
        let mut all = tools();
        all.push(Tool {
            kind: "function".into(),
            function: crate::openai::ToolFunction {
                name: "shell".into(),
                description: None,
                parameters: json!({"type": "object", "required": ["command"], "properties": {
                    "command": {"type": "string"}
                }}),
            },
        });
        all
    }

    #[test]
    fn parse_codeblock_markup_like_a_fence() {
        assert_eq!(
            parse_reply(CODEBLOCK_SAMPLE, &with_shell()),
            ParseOutcome::Calls {
                prefix: String::new(),
                calls: vec![(
                    "shell".into(),
                    json!({"command": "curl -fsSL https://example.invalid/install.sh | sh"})
                )],
            }
        );
        // Single quotes, other attributes, a body over several lines,
        // HTML entities, prose before and after.
        let text = "Смотрю файл.\n<CodeBlock className=\"x\" language='tool_calls' wrap>\n[{&quot;name&quot;: &quot;read&quot;, &quot;arguments&quot;: {&quot;path&quot;: &quot;a&lt;b&gt;&amp;c&quot;}}]\n</CodeBlock>\nЖду.";
        assert_eq!(
            parse_reply(text, &tools()),
            ParseOutcome::Calls {
                prefix: "Смотрю файл.".into(),
                calls: vec![("read".into(), json!({"path": "a<b>&c"}))],
            }
        );
        // `<CodeBlockX>` is another tag.
        let other = "<CodeBlockX language=\"tool_calls\">[]</CodeBlockX>";
        assert_eq!(
            parse_reply(other, &tools()),
            ParseOutcome::Text(other.into())
        );
    }

    #[test]
    fn parse_codeblock_with_another_language_stays_text() {
        let text = "Пример:\n<CodeBlock language=\"python\">print(\"hi\")\n</CodeBlock>\nВсё.";
        match parse_reply(text, &tools()) {
            ParseOutcome::Text(t) => {
                assert!(t.starts_with("Пример:"), "{}", t);
                assert!(t.contains("```python\nprint(\"hi\")\n```"), "{}", t);
                assert!(t.ends_with("Всё."), "{}", t);
                assert!(!t.contains("CodeBlock"), "{}", t);
            }
            other => panic!("{:?}", other),
        }
        // Calls under the wrong label are caught as for fences.
        let wrong = "<CodeBlock language=\"json\">[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]</CodeBlock>";
        assert_eq!(
            invalid(wrong),
            "вызовы оформлены блоком «json», нужна метка tool_calls"
        );
    }

    #[test]
    fn codeblock_body_is_unescaped_only_when_it_looks_escaped() {
        // A raw body (as seen live): literal entities in file content stay.
        let raw = r#"<CodeBlock language="tool_calls">[{"name":"patch","arguments":{"patchText":"+<p>a &lt; b &amp;&amp; c</p>"}}]</CodeBlock>"#;
        assert_eq!(
            calls(raw),
            vec![(
                "patch".into(),
                json!({"patchText": "+<p>a &lt; b &amp;&amp; c</p>"})
            )]
        );
        // A raw body with a stray `&` is raw too.
        let stray = r#"<CodeBlock language="tool_calls">[{"name":"patch","arguments":{"patchText":"a &amp; b & c"}}]</CodeBlock>"#;
        assert_eq!(calls(stray)[0].1, json!({"patchText": "a &amp; b & c"}));
        // An escaped body decodes, numeric entities included.
        let escaped = "<CodeBlock language=\"tool_calls\">[{&quot;name&quot;:&quot;patch&quot;,&quot;arguments&quot;:{&quot;patchText&quot;:&quot;&lt;p&gt;a &amp;lt; b&#39;&#x21;&lt;/p&gt;&quot;}}]</CodeBlock>";
        assert_eq!(
            calls(escaped)[0].1,
            json!({"patchText": "<p>a &lt; b'!</p>"})
        );
    }

    #[test]
    fn codeblock_needs_a_language_and_pairs_with_the_nearest_open_tag() {
        let call = "[{\"name\": \"read\", \"arguments\": {\"path\": \"b\"}}]";
        for mention in [
            "Не пиши <CodeBlock> вручную. Вызов:\n",
            "Тег <CodeBlock language=\"x\"> здесь только упомянут.\n",
            "Тег <CodeBlock language=\"x\"> упомянут, и сразу ",
        ] {
            let text = format!(
                "{}<CodeBlock language=\"tool_calls\">{}</CodeBlock>",
                mention, call
            );
            match parse_reply(&text, &tools()) {
                ParseOutcome::Calls { prefix, calls } => {
                    assert!(prefix.contains("<CodeBlock"), "{:?}: {}", mention, prefix);
                    assert_eq!(calls, vec![("read".into(), json!({"path": "b"}))]);
                }
                other => panic!("{:?}: {:?}", mention, other),
            }
        }
        // No language: not a block.
        let bare = format!("<CodeBlock>{}</CodeBlock>", call);
        assert_eq!(
            parse_reply(&bare, &tools()),
            ParseOutcome::Text(bare.clone())
        );
    }

    #[test]
    fn parse_mixed_fence_and_codeblock() {
        let call = "[{\"name\": \"read\", \"arguments\": {\"path\": \"b\"}}]";
        let text = format!(
            "Пример:\n```json\n{{\"a\": 1}}\n```\nВызов:\n<CodeBlock language=\"tool_calls\">{}</CodeBlock>",
            call
        );
        match parse_reply(&text, &tools()) {
            ParseOutcome::Calls { prefix, calls } => {
                assert_eq!(prefix, "Пример:\n```json\n{\"a\": 1}\n```\nВызов:");
                assert_eq!(calls, vec![("read".into(), json!({"path": "b"}))]);
            }
            other => panic!("{:?}", other),
        }
        // A fence and a CodeBlock both labelled tool_calls are two blocks
        // (the same block twice over would be a doubled answer).
        let other = "[{\"name\": \"read\", \"arguments\": {\"path\": \"c\"}}]";
        let two = format!(
            "```tool_calls\n{}\n```\n<CodeBlock language=\"tool_calls\">{}</CodeBlock>",
            call, other
        );
        assert!(invalid(&two).contains("ровно один"));
        // Markup shown inside a fenced example is part of that example.
        let example = format!(
            "Так выглядит разметка:\n```jsx\n<CodeBlock language=\"tool_calls\">{}</CodeBlock>\n```",
            call
        );
        assert_eq!(
            parse_reply(&example, &tools()),
            ParseOutcome::Text(example.clone())
        );
    }

    #[test]
    fn autolinked_urls_in_arguments_are_unwrapped() {
        let block = |args: Value| {
            format!(
                "```tool_calls\n{}\n```",
                json!([{"name": "read", "arguments": args}])
            )
        };
        let got = calls(&block(json!({
            "path": "see [https://a.example/x_(y)](https://a.example/x_(y)) and [http://b.example](http://b.example)"
        })));
        assert_eq!(
            got[0].1,
            json!({"path": "see https://a.example/x_(y) and http://b.example"})
        );
        // Only identical text and URL, and only http(s); the rest stays.
        for kept in [
            "[docs](https://a.example)",
            "[https://a.example](https://b.example)",
            "[ftp://a.example](ftp://a.example)",
            "[https://a.example](https://a.example",
            "[x] (y)",
        ] {
            assert_eq!(
                calls(&block(json!({ "path": kept })))[0].1,
                json!({ "path": kept })
            );
        }
    }

    /// OpenCode's real `question` tool, from the captured first request.
    fn question_tool() -> Vec<Tool> {
        let req: crate::openai::ChatRequest =
            serde_json::from_str(include_str!("../tests/fixtures/opencode-first.json")).unwrap();
        let tools: Vec<Tool> = req.tools.unwrap();
        let question: Vec<Tool> = tools
            .into_iter()
            .filter(|t| t.function.name == "question")
            .collect();
        assert_eq!(question.len(), 1, "the capture has the question tool");
        question
    }

    fn ask_question(arguments: Value) -> ParseOutcome {
        parse_reply(
            &format!(
                "```tool_calls\n{}\n```",
                json!([{"name": "question", "arguments": arguments}])
            ),
            &question_tool(),
        )
    }

    fn question_problem(arguments: Value) -> String {
        match ask_question(arguments) {
            ParseOutcome::Invalid(problem) => problem,
            other => panic!("expected Invalid, got {:?}", other),
        }
    }

    #[test]
    fn nested_arguments_are_checked_with_their_path() {
        let problem = question_problem(json!({"questions": [{
            "question": "Какую операцию?",
            "header": "Операция",
            "options": [
                {"label": "деление", "description": "a / b"},
                {"label": "степень"},
                {"label": 3, "description": "x"}
            ]
        }]}));
        assert!(
            problem.contains("questions[0].options[1].description: нет обязательного поля"),
            "{}",
            problem
        );
        assert!(
            problem.contains("questions[0].options[2].label: должно быть string, а пришло integer"),
            "{}",
            problem
        );
        assert!(problem.starts_with("вызов 1 (question): "), "{}", problem);
        let problem = question_problem(
            json!({"questions": [{"question": "?", "header": "h", "options": "да/нет"}]}),
        );
        assert!(
            problem.contains("questions[0].options: должно быть array, а пришло string"),
            "{}",
            problem
        );
        let problem = question_problem(json!({"questions": [{"question": "?", "options": []}]}));
        assert!(
            problem.contains("questions[0].header: нет обязательного поля"),
            "{}",
            problem
        );
        // A good call passes; null for an optional nested field is dropped.
        assert_eq!(
            ask_question(
                json!({"questions": [{"question": "?", "header": "h", "multiple": null,
                "options": [{"label": "да", "description": "согласен"}]}]})
            ),
            ParseOutcome::Calls {
                prefix: String::new(),
                calls: vec![(
                    "question".into(),
                    json!({"questions": [{"question": "?", "header": "h", "options": [
                        {"label": "да", "description": "согласен"}
                    ]}]})
                )],
            }
        );
    }

    #[test]
    fn unknown_keys_are_dropped_and_too_short_arrays_named() {
        // OpenCode's schemas say `additionalProperties: false` everywhere
        // (a serializer default); OpenCode itself most likely ignores
        // unknown keys (inferred, not observed), so the gateway drops them at
        // every depth instead of rejecting.
        assert_eq!(
            ask_question(json!({"questions": [{
                "question": "?", "header": "h", "hint": "x",
                "options": [{"label": "да", "description": "согласен", "value": 1}]
            }], "timeout": 5})),
            ParseOutcome::Calls {
                prefix: String::new(),
                calls: vec![(
                    "question".into(),
                    json!({"questions": [{"question": "?", "header": "h",
                        "options": [{"label": "да", "description": "согласен"}]}]})
                )],
            }
        );
        // `minItems` is still named.
        let problem = question_problem(json!({"questions": []}));
        assert!(
            problem.contains("questions: нужно не меньше 1 элементов, а пришло 0"),
            "{}",
            problem
        );
        // Without `additionalProperties: false` extra keys are kept.
        assert_eq!(
            calls("```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\", \"extra\": 1}}]\n```"),
            vec![("read".into(), json!({"path": "a", "extra": 1}))]
        );
    }

    #[test]
    fn unconverted_tool_calls_codeblock_asks_for_repair() {
        // The JSON holds a CodeBlock of its own: the outer tag cannot be
        // paired and stays as markup. Silent text would lose the call.
        let text = r#"<CodeBlock language="tool_calls">[{"name":"patch","arguments":{"patchText":"+<CodeBlock language=\"ts\">x</CodeBlock>"}}]</CodeBlock>"#;
        let problem = invalid(text);
        assert!(problem.contains("```tool_calls"), "{}", problem);
        // Inside a fenced example it is only an example.
        let example = "```jsx\n<CodeBlock language=\"tool_calls\">[</CodeBlock>\n```";
        assert_eq!(
            parse_reply(example, &tools()),
            ParseOutcome::Text(example.into())
        );
    }

    #[test]
    fn corpus_calls_come_only_from_top_level_tool_calls_blocks() {
        use cgpt_bridge_protocol::fence::corpus::{Expected, FENCE_CORPUS};
        for case in FENCE_CORPUS {
            let ours: Vec<&Expected> = case
                .blocks
                .iter()
                .filter(|e| {
                    e.info
                        .split_whitespace()
                        .next()
                        .is_some_and(|w| w.eq_ignore_ascii_case(LABEL))
                })
                .collect();
            let got = parse_reply(case.input, &tools());
            match ours.as_slice() {
                [] => assert_eq!(
                    got,
                    ParseOutcome::Text(case.input.trim().into()),
                    "{}",
                    case.name
                ),
                [e] if e.closed => match got {
                    ParseOutcome::Calls { calls, .. } => assert_eq!(
                        calls,
                        vec![("read".to_string(), json!({"path": "a"}))],
                        "{}",
                        case.name
                    ),
                    other => panic!("{}: expected Calls, got {:?}", case.name, other),
                },
                _ => assert!(
                    matches!(got, ParseOutcome::Invalid(_)),
                    "{}: {:?}",
                    case.name,
                    got
                ),
            }
        }
    }

    #[test]
    fn repair_request_names_the_problem_and_the_format() {
        let r = repair_request("вызов 1 (read): нет обязательного параметра «path»");
        assert!(r.contains("вызов 1 (read): нет обязательного параметра «path»"));
        assert!(r.contains("```tool_calls"));
    }

    /// OpenCode's real tools, from the captured first request.
    fn opencode_tools() -> Vec<Tool> {
        let req: crate::openai::ChatRequest =
            serde_json::from_str(include_str!("../tests/fixtures/opencode-first.json")).unwrap();
        req.tools.unwrap()
    }

    /// What a fixture answer must give.
    enum Want {
        /// These calls (name, arguments), and the text before them.
        Calls(&'static str, Vec<(&'static str, Value)>),
        /// A repair request (`Invalid`) naming all of these.
        Invalid(&'static [&'static str]),
        /// A repair request for a call in another form, naming all of these;
        /// the answer as text if the repair fails.
        Shaped(&'static [&'static str]),
        /// The answer as text.
        Text,
    }

    /// The answers under `tests/fixtures/replies/`, one per file, with what
    /// the parser must make of them (see `tests/fixtures/replies`):
    /// live cases name their date, the others are built after a live case
    /// or a form ChatGPT is known to use.
    fn fixtures() -> Vec<(&'static str, &'static str, Want)> {
        macro_rules! fixture {
            ($name:literal) => {
                (
                    $name,
                    include_str!(concat!("../tests/fixtures/replies/", $name, ".txt")),
                )
            };
        }
        let with = |(name, text): (&'static str, &'static str), want| (name, text, want);
        vec![
            // After acceptance 02.11, step s12: the forbidden `curl … | sh`
            // written as a `json` block under the old tool name `bash`, which
            // went to OpenCode as text.
            with(
                fixture!("json-labelled-old-tool-name"),
                Want::Shaped(&["блок «json»", "инструмента «bash» нет", "shell"]),
            ),
            // The same in the gateway's own form, only labelled `json`.
            with(
                fixture!("json-labelled-call"),
                Want::Invalid(&["вызовы оформлены блоком «json», нужна метка tool_calls"]),
            ),
            // The OpenAI wire form in a `json` block.
            with(
                fixture!("json-labelled-openai-form"),
                Want::Shaped(&["блок «json»"]),
            ),
            // A line naming `shell`, then its arguments alone in a block.
            with(
                fixture!("tool-name-then-json-block"),
                Want::Shaped(&["аргументы «shell» в блоке «json»"]),
            ),
            // `read {...}` in prose.
            with(
                fixture!("tool-name-inline"),
                Want::Shaped(&["«read {...}» текстом"]),
            ),
            // The gateway's own form without any block.
            with(
                fixture!("bare-json-call"),
                Want::Invalid(&["JSON без блока"]),
            ),
            // The whole answer twice, label `tool_calls id="..."` (doubling
            // seen live, cause not confirmed): the calls of one copy.
            with(
                fixture!("doubled-tool-calls-id"),
                Want::Calls(
                    "Посмотрю, где описан конфиг шлюза, и найду все его ключи.",
                    vec![
                        ("read", json!({"path": "gateway/src/config.rs"})),
                        (
                            "grep",
                            json!({"pattern": "KNOWN_KEYS", "path": "gateway/src"}),
                        ),
                    ],
                ),
            ),
            // Live 05.11 (Windows): every line of the call in inline code.
            with(
                fixture!("inline-code-lines"),
                Want::Calls(
                    "Сначала прочитаю файл сканера.",
                    vec![(
                        "read",
                        json!({"path": "C:\\scanner opencode\\Scanner.cs", "offset": 1}),
                    )],
                ),
            ),
            // Live 05.11: attributes after the label.
            with(
                fixture!("label-with-attributes"),
                Want::Calls(
                    "Подготовлю ревью: посмотрю изменённые файлы.",
                    vec![("shell", json!({"command": "git diff --stat main...HEAD"}))],
                ),
            ),
            // Prose naming tools, a package.json, a tool's definition, JS
            // code calling `read(...)`, a glob pattern alone: no call.
            with(fixture!("prose-and-json-not-calls"), Want::Text),
        ]
    }

    #[test]
    fn fixture_answers_are_read_as_recorded() {
        let tools = opencode_tools();
        for (name, text, want) in fixtures() {
            let got = parse_reply(text, &tools);
            match (&want, &got) {
                (
                    Want::Calls(prefix, calls),
                    ParseOutcome::Calls {
                        prefix: p,
                        calls: c,
                    },
                ) => {
                    assert_eq!(p, prefix, "{}", name);
                    let calls: Vec<(String, Value)> = calls
                        .iter()
                        .map(|(n, a)| (n.to_string(), a.clone()))
                        .collect();
                    assert_eq!(c, &calls, "{}", name);
                }
                (Want::Invalid(words), ParseOutcome::Invalid(problem))
                | (Want::Shaped(words), ParseOutcome::CallShaped { problem, .. }) => {
                    for w in *words {
                        assert!(problem.contains(w), "{}: {:?} not in {}", name, w, problem);
                    }
                    // Repair asks for the block, and nothing of it is run.
                    assert!(
                        repair_request(problem).contains("```tool_calls"),
                        "{}",
                        name
                    );
                }
                (Want::Text, ParseOutcome::Text(t)) => assert_eq!(t, text.trim(), "{}", name),
                _ => panic!("{}: got {:?}", name, got),
            }
            if let ParseOutcome::CallShaped { text: t, .. } = &got {
                assert_eq!(t, text.trim(), "{}: the text as it came", name);
            }
        }
    }

    #[test]
    fn every_fixture_file_is_in_the_table() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replies");
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .trim_end_matches(".txt")
                    .to_string()
            })
            .collect();
        files.sort();
        let mut listed: Vec<String> = fixtures().iter().map(|(n, _, _)| n.to_string()).collect();
        listed.sort();
        assert_eq!(files, listed);
    }

    #[test]
    fn call_shaped_forms_ask_for_repair_and_near_misses_stay_text() {
        let tools = opencode_tools();
        let shaped = |text: &str| match parse_reply(text, &tools) {
            ParseOutcome::CallShaped { problem, .. } => problem,
            other => panic!("{:?}: expected CallShaped, got {:?}", text, other),
        };
        // Other keys for the name and the arguments, with or without a fence,
        // closed or not, `functions.` before the name.
        shaped("```\n{\"tool\": \"read\", \"args\": {\"path\": \"a\"}}\n```");
        shaped(
            "```javascript\n{\"tool_name\": \"glob\", \"parameters\": {\"pattern\": \"*\"}}\n```",
        );
        shaped("```json\n{\"recipient_name\": \"functions.read\", \"input\": {\"path\": \"a\"}}\n");
        shaped("Вызываю:\n{\"tool\": \"shell\", \"params\": {\"command\": \"ls\"}}\n");
        shaped("`{\"function\": {\"name\": \"read\", \"arguments\": \"{}\"}}`");
        // A tool name with its arguments: after `(` or `:`, over two lines.
        shaped("Читаю: read({\"path\": \"a\"})");
        shaped("shell: {\"command\":\n  \"ls -la\"}");
        // An unknown name in the gateway's own form names the known ones.
        let problem =
            shaped("```json\n[{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}]\n```");
        assert!(
            problem.contains("доступны: glob, grep, patch"),
            "{}",
            problem
        );
        // Unclosed, the gateway's own form: the label is wrong all the same.
        assert!(matches!(
            parse_reply(
                "```json\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]\n",
                &tools
            ),
            ParseOutcome::Invalid(_)
        ));

        for text in [
            // Prose about tools.
            "Инструмент read читает файл, shell запускает команду.",
            "Я вызову read, когда ты пришлёшь путь: {путь}.",
            // Arguments that do not fit the tool named before them.
            "read {\"file\": \"a\"}",
            "shell {\"command\": 5}",
            "Пример для read:\n```json\n{\"path\": \"a\", \"encoding\": \"utf8\"}\n```",
            // An object naming a tool, but no arguments.
            "```json\n{\"tool\": \"read\"}\n```",
            // A tool's definition.
            "```json\n{\"tool\": \"read\", \"parameters\": {\"type\": \"object\", \"properties\": {}}}\n```",
            // Arguments-like JSON with no tool named before it.
            "Маска:\n{\"pattern\": \"*.rs\"}",
            // `read` inside another word or a path, and in code.
            "reader {\"path\": \"a\"}\n./read {\"path\": \"a\"}",
            "```js\nread({\"path\": \"a\"})\n```",
        ] {
            assert_eq!(
                parse_reply(text, &tools),
                ParseOutcome::Text(text.trim().to_string()),
                "{:?}",
                text
            );
        }
    }

    #[test]
    fn a_doubled_answer_gives_the_calls_once_and_other_doubles_are_read_as_they_are() {
        let tools = opencode_tools();
        let one =
            "Читаю.\n```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]\n```";
        // The copies meet right after the closing fence: one broken block,
        // but the answer is still one answer twice.
        let glued = format!("{}{}", one, one);
        for text in [format!("{}\n{}", one, one), glued] {
            assert!(is_doubled(&text));
            match parse_reply(&text, &tools) {
                ParseOutcome::Calls { prefix, calls } => {
                    assert_eq!(prefix, "Читаю.");
                    assert_eq!(calls, vec![("read".into(), json!({"path": "a"}))]);
                }
                other => panic!("{:?}", other),
            }
        }
        // Copies that differ only by whitespace inside an argument are two
        // different calls: two blocks, a repair, nothing run.
        let spaced = "Удаляю.\n```tool_calls\n[{\"name\": \"shell\", \"arguments\": {\"command\": \"rm -r ./build /tmp/x\"}}]\n```";
        let joined = spaced.replace("./build /tmp/x", "./build/tmp/x");
        let near = format!("{}\n{}", spaced, joined);
        assert!(!is_doubled(&near));
        assert!(matches!(
            parse_reply(&near, &tools),
            ParseOutcome::Invalid(ref p) if p.contains("ровно один")
        ));
        // Whitespace around the copies and at the seam does not count.
        let padded = format!("  {}\n\n \t{}\n", spaced, spaced);
        assert!(matches!(
            parse_reply(&padded, &tools),
            ParseOutcome::Calls { ref calls, .. } if calls.len() == 1
        ));
        // Two different blocks stay two blocks.
        let two = format!("{}\n{}", one, one.replace("\"a\"", "\"b\""));
        assert!(!is_doubled(&two));
        assert!(matches!(
            parse_reply(&two, &tools),
            ParseOutcome::Invalid(_)
        ));
        // Doubled text without calls is left as it is.
        assert_eq!(
            parse_reply("Готово. Готово.", &tools),
            ParseOutcome::Text("Готово. Готово.".into())
        );
        assert!(!is_doubled("Готово. Готово!"));
    }

    #[test]
    fn mentions_calls_flags_text_that_talks_in_calls() {
        let tools = opencode_tools();
        assert!(mentions_calls("пример: ```tool_calls ...", &tools));
        assert!(mentions_calls(
            "{\"name\": \"x\", \"arguments\": {}}",
            &tools
        ));
        assert!(mentions_calls("{\"name\": \"read\"}", &tools));
        assert!(!mentions_calls("Готово, файл прочитан (read).", &tools));
        assert!(!mentions_calls("{\"name\": \"app\"}", &tools));
    }
}
