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
                .find(|b| b.closed && looks_like_calls(&b.body, tools))
            {
                let label = if b.label().is_empty() {
                    "без метки"
                } else {
                    b.label()
                };
                return ParseOutcome::Invalid(format!(
                    "вызовы оформлены блоком «{}», нужна метка tool_calls",
                    label
                ));
            }
            if stray_calls_tag {
                return ParseOutcome::Invalid(
                    "блок tool_calls пришёл разметкой <CodeBlock>, которую не удалось \
                     разобрать: пришли вызовы обычным блоком ```tool_calls"
                        .to_string(),
                );
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
    match parse_calls(&block.body, tools) {
        Ok(calls) => ParseOutcome::Calls {
            prefix: text[..block.start].trim().to_string(),
            calls,
        },
        Err(problem) => ParseOutcome::Invalid(problem),
    }
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
    let known = |item: &Value| {
        item.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| tools.iter().any(|t| t.function.name == n))
    };
    match serde_json::from_str::<Value>(body.trim()) {
        Ok(Value::Object(obj)) => obj.contains_key("arguments") && known(&Value::Object(obj)),
        Ok(Value::Array(items)) => {
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
        // A fence and a CodeBlock both labelled tool_calls are two blocks.
        let two = format!(
            "```tool_calls\n{}\n```\n<CodeBlock language=\"tool_calls\">{}</CodeBlock>",
            call, call
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
}
