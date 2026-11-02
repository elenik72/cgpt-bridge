//! Request-to-chat translation: the gateway rules for ChatGPT, compact tool
//! schemas, the bootstrap pack for a new chat, deltas for a known chat and
//! splitting into parts. Everything that leaves for ChatGPT is redacted
//! before it is measured, cut or split.

use std::collections::HashMap;

use cgpt_bridge_core::limits::MAX_MESSAGE_CHARS;
use cgpt_bridge_core::redact::redact;
use serde_json::{Map, Value};

use crate::openai::{ChatRequest, Message, Tool, ToolCall};
use crate::safety;

const RULES: &str = "Правила шлюза

Ты работаешь как модель агента для программирования. Инструменты выполняет программа пользователя: ты решаешь, какие вызвать, а их результаты приходят следующим сообщением.

Чтобы вызвать инструменты, ответь одним fenced-блоком с меткой tool_calls. Внутри — JSON-массив вызовов:

```tool_calls
[{\"name\": \"имя_инструмента\", \"arguments\": {\"параметр\": \"значение\"}}]
```

- Перед блоком можно написать короткий текст.
- Несколько вызовов в одном блоке допустимы. Блок tool_calls в ответе — только один.
- Имена инструментов и параметров — только из списка инструментов ниже; обязательные параметры указывай всегда.
- Без блока ответ считается окончательным текстом для пользователя.
- Не используй Canvas, не вызывай свои встроенные инструменты ChatGPT и не показывай свои интерактивные элементы (кнопки, переключатели, формы): программа пользователя видит только текст. Чтобы спросить пользователя, вызови инструмент question, если он есть в списке.
- Отвечай на языке пользователя.";

/// The one-line format reminder at the end of every message.
pub const REMINDER: &str = "Напоминание: чтобы вызвать инструменты, ответь одним блоком ```tool_calls с JSON-массивом [{\"name\": \"...\", \"arguments\": {...}}]; без блока ответ — окончательный текст для пользователя.";
/// Opens the first message to a chat whose previous turn was cut off.
pub const INTERRUPTED_NOTE: &str =
    "Предыдущее сообщение прервано, ответ на него не нужен; продолжаем с этого места.";

/// Opens the rest of a message whose first `answered` parts the chat has
/// (the sending was cut off after them): those parts still count.
pub fn continued_note(answered: usize) -> String {
    format!(
        "Продолжение сообщения выше: отправка прервалась после части {}, полученные части \
         остаются в силе. Ниже — остальное с части {} (она повторяется с начала); ответь на \
         сообщение целиком после последней части.",
        answered,
        answered + 1
    )
}
const UPDATED_RULES_TITLE: &str = "## Правила и инструменты обновлены";

/// Older tool results in a bootstrap are cut to this many characters.
const OLD_RESULT_CHARS: usize = 2_000;
/// The latest tool results that go into a bootstrap in full.
const FULL_RESULTS: usize = 3;
/// Room for the "часть N из M" header in each part.
const HEADER_RESERVE: usize = 100;
const PART_BODY: usize = MAX_MESSAGE_CHARS - HEADER_RESERVE;
// A part goes to the host in one frame: as JSON a character takes at most 6
// bytes (`\u001f`), plus room for the request's other fields.
const _: () = assert!(
    MAX_MESSAGE_CHARS * 6 + 4096 <= cgpt_bridge_protocol::MAX_FRAME_BYTES as usize,
    "a full part would not fit in one bridge frame"
);
/// A part is cut after the last line break among its last characters, if any.
const LINE_WINDOW: usize = 2_000;
/// The longest text `split` is guaranteed to send in two parts.
const BOOTSTRAP_BUDGET: usize = 2 * PART_BODY - LINE_WINDOW;
/// Upper bound of the length of `clip_note`.
const CLIP_NOTE_MAX: usize = 80;

/// The rules text that opens every bootstrap pack (read by tests).
#[cfg(test)]
pub fn gateway_rules() -> &'static str {
    RULES
}

/// Name, description and a compact parameter list of every tool shown to
/// ChatGPT. `execute` is shown only when `allow` names it.
pub fn compact_tools(tools: &[Tool], allow: &[String]) -> String {
    let shown: Vec<String> = safety::visible_tools(tools, allow)
        .iter()
        .map(compact_tool)
        .collect();
    if shown.is_empty() {
        return "Инструментов нет: отвечай текстом.".to_string();
    }
    shown.join("\n\n")
}

fn compact_tool(tool: &Tool) -> String {
    let f = &tool.function;
    let mut out = format!("### {}\n", f.name);
    if let Some(d) = f.description.as_deref().map(str::trim_end) {
        if !d.trim().is_empty() {
            out.push_str(d);
            out.push('\n');
        }
    }
    let params = param_lines(&f.parameters);
    if params.is_empty() {
        out.push_str("Параметры: нет");
    } else {
        out.push_str("Параметры:\n");
        out.push_str(&params.join("\n"));
    }
    out
}

/// `- name: type [required] [enum: a|b] — description`, one line per
/// parameter. Required parameters come first in the schema's `required`
/// order, then the rest by name (`serde_json` keeps object keys sorted).
fn param_lines(schema: &Value) -> Vec<String> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required = required_names(schema);
    ordered_names(props, &required)
        .into_iter()
        .map(|name| {
            let p = &props[name];
            let mut line = format!("- {}: {}", name, type_name(p, 0));
            if required.contains(&name) {
                line.push_str(" [required]");
            }
            if let Some(values) = enum_values(p) {
                line.push_str(&format!(" [enum: {}]", values));
            }
            let description = p
                .get("description")
                .and_then(Value::as_str)
                .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            if !description.is_empty() {
                line.push_str(" — ");
                line.push_str(&description);
            }
            line
        })
        .collect()
}

fn required_names(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn ordered_names<'a>(props: &'a Map<String, Value>, required: &[&'a str]) -> Vec<&'a str> {
    let mut names: Vec<&str> = required
        .iter()
        .copied()
        .filter(|n| props.contains_key(*n))
        .collect();
    names.extend(
        props
            .keys()
            .map(String::as_str)
            .filter(|n| !required.contains(n)),
    );
    names
}

/// The parameter's type: `string`, `string|null`, `array<string>`, and for
/// objects one level of `{field: type, optional?: type}`.
fn type_name(schema: &Value, depth: usize) -> String {
    match schema.get("type") {
        Some(Value::String(t)) => named_type(t, schema, depth),
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .map(|t| named_type(t, schema, depth))
            .collect::<Vec<_>>()
            .join("|"),
        _ => {
            if let Some(variants) = schema
                .get("anyOf")
                .or_else(|| schema.get("oneOf"))
                .and_then(Value::as_array)
            {
                let mut names: Vec<String> = Vec::new();
                for v in variants {
                    let n = type_name(v, depth);
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
                return names.join("|");
            }
            if schema.get("properties").is_some() {
                return object_summary(schema, depth);
            }
            "any".to_string()
        }
    }
}

fn named_type(t: &str, schema: &Value, depth: usize) -> String {
    match t {
        "array" => match schema.get("items") {
            Some(items) => format!("array<{}>", type_name(items, depth)),
            None => "array".to_string(),
        },
        "object" => object_summary(schema, depth),
        other => other.to_string(),
    }
}

fn object_summary(schema: &Value, depth: usize) -> String {
    let props = match schema.get("properties").and_then(Value::as_object) {
        Some(p) if !p.is_empty() && depth == 0 => p,
        _ => return "object".to_string(),
    };
    let required = required_names(schema);
    let fields: Vec<String> = ordered_names(props, &required)
        .into_iter()
        .map(|n| {
            let optional = if required.contains(&n) { "" } else { "?" };
            format!("{}{}: {}", n, optional, type_name(&props[n], depth + 1))
        })
        .collect();
    format!("{{{}}}", fields.join(", "))
}

fn enum_values(schema: &Value) -> Option<String> {
    let values = schema
        .get("enum")
        .or_else(|| schema.get("items").and_then(|i| i.get("enum")))
        .and_then(Value::as_array)?;
    Some(
        values
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join("|"),
    )
}

/// The first message of a new chat: rules, OpenCode's system prompt, tools,
/// then the history, in at most two parts:
/// - tool results older than the last 3 are cut to 2 000 characters;
/// - if the latest step (assistant message and its results) does not fit
///   together with everything that is never dropped, its results are cut so
///   that the whole history fits, but not below 2 000 characters each;
/// - then the oldest steps are dropped, with the note «N ранних ходов
///   опущено» where they were.
///
/// User messages (the original task included), the system prompt and the
/// latest step are never dropped. When they alone exceed two parts, the
/// pack is sent in more parts; every part is still at most
/// `MAX_MESSAGE_CHARS`.
pub fn bootstrap(req: &ChatRequest, allow: &[String]) -> Vec<String> {
    let system: Vec<String> = req
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.text())
        .filter(|t| !t.trim().is_empty())
        .collect();
    let mut head = format!("{}\n\n", RULES);
    if !system.is_empty() {
        head.push_str("## Системная подсказка OpenCode\n\n");
        head.push_str(&clean(&system.join("\n\n")));
        head.push_str("\n\n");
    }
    head.push_str("## Инструменты\n\n");
    head.push_str(&clean(&compact_tools(
        req.tools.as_deref().unwrap_or(&[]),
        allow,
    )));
    head.push_str("\n\n## История и задача пользователя\n\n");
    let tail = format!("\n\n{}", REMINDER);

    let history: Vec<&Message> = req.messages.iter().filter(|m| m.role != "system").collect();
    let names = call_names(history.iter().copied());
    let mut items: Vec<Item> = Vec::new();
    // Steps are numbered from 0; results before any assistant message join
    // step 0.
    let mut step: Option<usize> = None;
    for m in &history {
        let item_step = match m.role.as_str() {
            "assistant" => {
                step = Some(step.map_or(0, |s| s + 1));
                step
            }
            "tool" => Some(step.unwrap_or(0)),
            _ => None,
        };
        items.push(Item::new(m, &names, item_step));
    }
    let last_step = step.unwrap_or(0);
    let results: Vec<usize> = (0..items.len()).filter(|&i| items[i].is_result).collect();
    for &i in results.iter().rev().skip(FULL_RESULTS) {
        items[i].cap = Some(OLD_RESULT_CHARS);
    }

    let fixed = chars(&head) + chars(&tail);
    if fixed + pack_chars(&items, last_step) > BOOTSTRAP_BUDGET {
        let total = fixed + pack_chars(&items, 0);
        cap_latest_results(&mut items, last_step, total);
    }
    let mut first_kept = 0;
    while first_kept < last_step && fixed + pack_chars(&items, first_kept) > BOOTSTRAP_BUDGET {
        first_kept += 1;
    }
    split(&format!(
        "{}{}{}",
        head,
        pack_blocks(&items, first_kept).join("\n\n"),
        tail
    ))
}

/// Cut the full results of `step` so that the whole pack (`total` now) fits:
/// short ones stay whole, the rest get an equal cap of at least 2 000.
fn cap_latest_results(items: &mut [Item], step: usize, total: usize) {
    let full: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].is_result && items[i].cap.is_none() && items[i].step == Some(step))
        .collect();
    // The pack without the bodies of these results.
    let rest = total - full.iter().map(|&i| items[i].body_chars).sum::<usize>();
    let mut left = BOOTSTRAP_BUDGET.saturating_sub(rest);
    let mut by_len: Vec<(usize, usize)> = full.iter().map(|&i| (items[i].body_chars, i)).collect();
    by_len.sort();
    for (n, &(len, _)) in by_len.iter().enumerate() {
        let share = left / (by_len.len() - n);
        if len <= share {
            left -= len;
        } else {
            let cap = share.saturating_sub(CLIP_NOTE_MAX).max(OLD_RESULT_CHARS);
            for &(_, j) in &by_len[n..] {
                items[j].cap = Some(cap);
            }
            return;
        }
    }
}

/// The history blocks with the steps before `first_kept` dropped: user
/// messages stay, and the note goes where the first kept step starts.
fn pack_blocks(items: &[Item], first_kept: usize) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut noted = first_kept == 0;
    for item in items {
        match item.step {
            Some(s) if s < first_kept => continue,
            Some(_) if !noted => {
                blocks.push(omitted_note(first_kept));
                noted = true;
            }
            _ => {}
        }
        blocks.push(item.render());
    }
    blocks
}

/// `pack_blocks(items, first_kept).join("\n\n")` in characters, without
/// rendering.
fn pack_chars(items: &[Item], first_kept: usize) -> usize {
    let mut lens: Vec<usize> = items
        .iter()
        .filter(|i| i.step.is_none_or(|s| s >= first_kept))
        .map(Item::rendered_chars)
        .collect();
    if first_kept > 0 {
        lens.push(chars(&omitted_note(first_kept)));
    }
    lens.iter().sum::<usize>() + 2 * lens.len().saturating_sub(1)
}

/// The parts of [`delta_text`].
#[cfg(test)]
pub fn delta(
    msgs: &[Message],
    note_interrupted: bool,
    updated_rules: Option<(&str, &str)>,
) -> Vec<String> {
    split_redacted(&delta_text(msgs, note_interrupted, updated_rules))
        .into_iter()
        .map(|(part, _)| part)
        .collect()
}

/// Messages ChatGPT has not seen yet, for a chat that already has the
/// history: tool results and user messages. Assistant messages are
/// ChatGPT's own replies: they are not sent back, only used to name the
/// tool of each result. `updated_rules` is the new system prompt and the new
/// `compact_tools` text when they changed. Redacted, not yet cut into parts.
pub fn delta_text(
    msgs: &[Message],
    note_interrupted: bool,
    updated_rules: Option<(&str, &str)>,
) -> String {
    let mut blocks: Vec<String> = Vec::new();
    if note_interrupted {
        blocks.push(INTERRUPTED_NOTE.to_string());
    }
    if let Some((system, tools)) = updated_rules {
        blocks.push(format!(
            "{}\n\n### Системная подсказка OpenCode\n\n{}\n\n### Инструменты\n\n{}",
            UPDATED_RULES_TITLE, system, tools
        ));
    }
    let names = call_names(msgs.iter());
    // All results of a delta are the latest step: when the delta is over the
    // bootstrap's budget, they are cut the same way.
    let mut items: Vec<Item> = msgs
        .iter()
        .filter(|m| m.role != "assistant" && m.role != "system")
        .map(|m| Item::new(m, &names, (m.role == "tool").then_some(0)))
        .collect();
    let fixed: usize = blocks.iter().map(|b| chars(b) + 2).sum::<usize>() + chars(REMINDER);
    let total = fixed + pack_chars(&items, 0) + 2;
    if total > BOOTSTRAP_BUDGET {
        cap_latest_results(&mut items, 0, total);
    }
    blocks.extend(items.iter().map(Item::render));
    blocks.push(REMINDER.to_string());
    redact(&blocks.join("\n\n"), "").stdout
}

/// Redact, then cut into parts of at most `MAX_MESSAGE_CHARS` characters.
/// A short text is one part as is. Longer text is cut at a line break when
/// one is near the limit (never inside a character); every part but the
/// last starts with "часть N из M, ответь словом ok".
pub fn split(text: &str) -> Vec<String> {
    split_redacted(&redact(text, "").stdout)
        .into_iter()
        .map(|(part, _)| part)
        .collect()
}

/// [`split`] of a text already redacted, each part with where its piece
/// of `text` ends (a byte offset).
pub fn split_redacted(text: &str) -> Vec<(String, usize)> {
    split_after(text, 0)
}

/// [`split_redacted`] of the rest of a message whose first `answered`
/// parts the chat has: its parts are numbered on from `answered + 1`, a
/// single one too.
pub fn split_after(text: &str, answered: usize) -> Vec<(String, usize)> {
    if answered == 0 && chars(text) <= MAX_MESSAGE_CHARS {
        return vec![(text.to_string(), text.len())];
    }
    let mut chunks: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest
            .char_indices()
            .nth(PART_BODY)
            .map(|(b, _)| b)
            .unwrap_or(rest.len());
        let cut = if end == rest.len() {
            end
        } else {
            match rest[..end].rfind('\n') {
                Some(nl) if chars(&rest[nl + 1..end]) < LINE_WINDOW => nl + 1,
                _ => end,
            }
        };
        chunks.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    let m = answered + chunks.len();
    let mut end = 0;
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            end += chunk.len();
            let n = answered + i + 1;
            let part = if n < m {
                format!("часть {} из {}, ответь словом ok\n\n{}", n, m, chunk)
            } else {
                format!("часть {} из {}, последняя\n\n{}", m, m, chunk)
            };
            (part, end)
        })
        .collect()
}

/// One history message as ChatGPT sees it: a header line and a body; the
/// body may be cut to `cap` characters.
struct Item {
    head: String,
    body: String,
    body_chars: usize,
    cap: Option<usize>,
    is_result: bool,
    /// The assistant step the message belongs to; `None` for user
    /// messages, which are never dropped.
    step: Option<usize>,
}

impl Item {
    /// The body is redacted here, before any cutting can split a secret.
    fn new(m: &Message, names: &HashMap<&str, &str>, step: Option<usize>) -> Item {
        let (head, body) = match m.role.as_str() {
            "tool" => {
                let id = m.tool_call_id.as_deref().unwrap_or("?");
                let head = match names.get(id) {
                    Some(name) => format!("[результат {} · {}]", id, name),
                    None => format!("[результат {}]", id),
                };
                (head, m.text())
            }
            "assistant" => {
                let mut body = m.text();
                if let Some(calls) = m.tool_calls.as_deref().filter(|c| !c.is_empty()) {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&calls_block(calls));
                }
                ("[ассистент]".to_string(), body)
            }
            "user" => ("[пользователь]".to_string(), m.text()),
            other => (format!("[{}]", other), m.text()),
        };
        let body = clean(&body);
        Item {
            head,
            body_chars: chars(&body),
            body,
            cap: None,
            is_result: m.role == "tool",
            step,
        }
    }

    fn render(&self) -> String {
        match self.cap {
            Some(cap) if self.body_chars > cap => {
                let kept: String = self.body.chars().take(cap).collect();
                format!("{}\n{}{}", self.head, kept, clip_note(cap, self.body_chars))
            }
            _ => format!("{}\n{}", self.head, self.body),
        }
    }

    /// `render().chars().count()` without rendering.
    fn rendered_chars(&self) -> usize {
        let body = match self.cap {
            Some(cap) if self.body_chars > cap => cap + chars(&clip_note(cap, self.body_chars)),
            _ => self.body_chars,
        };
        chars(&self.head) + 1 + body
    }
}

/// Earlier calls in the same format ChatGPT is asked to use.
fn calls_block(calls: &[ToolCall]) -> String {
    let items: Vec<String> = calls
        .iter()
        .map(|c| {
            let args = match serde_json::from_str::<Value>(&c.function.arguments) {
                Ok(args) => args.to_string(),
                Err(_) => "{} /* аргументы не разобраны */".to_string(),
            };
            format!(
                "{{\"name\": {}, \"arguments\": {}}}",
                Value::String(c.function.name.clone()),
                args
            )
        })
        .collect();
    format!("```tool_calls\n[{}]\n```", items.join(", "))
}

/// Tool name by call id, from the assistant messages' `tool_calls`.
fn call_names<'a>(msgs: impl Iterator<Item = &'a Message>) -> HashMap<&'a str, &'a str> {
    msgs.flat_map(|m| m.tool_calls.iter().flatten())
        .map(|c| (c.id.as_str(), c.function.name.as_str()))
        .collect()
}

fn omitted_note(dropped: usize) -> String {
    format!("[{} ранних ходов опущено]", dropped)
}

fn clip_note(cap: usize, total: usize) -> String {
    format!(
        "\n…[обрезано: показаны первые {} из {} символов]",
        cap, total
    )
}

fn clean(text: &str) -> String {
    redact(text, "").stdout
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{Content, FunctionCall, ToolCall, ToolFunction};
    use cgpt_bridge_core::limits::MAX_MESSAGE_CHARS;
    use cgpt_bridge_core::redact::redact;
    use serde_json::{json, Value};

    fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(Content::Text(text.into())),
            ..Default::default()
        }
    }

    fn calls(calls: &[(&str, &str, &str)]) -> Message {
        Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(
                calls
                    .iter()
                    .map(|(id, name, args)| ToolCall {
                        id: id.to_string(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: name.to_string(),
                            arguments: args.to_string(),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
        }
    }

    fn result(id: &str, text: &str) -> Message {
        Message {
            role: "tool".into(),
            content: Some(Content::Text(text.into())),
            tool_call_id: Some(id.into()),
            ..Default::default()
        }
    }

    fn tool(name: &str, parameters: Value) -> Tool {
        Tool {
            kind: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: Some(format!("{} tool", name)),
                parameters,
            },
        }
    }

    fn read_tool() -> Tool {
        tool(
            "read",
            json!({"type": "object", "required": ["path"], "properties": {
                "path": {"type": "string", "description": "File to read"},
                "offset": {"type": "integer", "minimum": 0}
            }}),
        )
    }

    fn request(messages: Vec<Message>, tools: Vec<Tool>) -> ChatRequest {
        ChatRequest {
            model: "chatgpt-web".into(),
            messages,
            tools: Some(tools),
            stream: Some(true),
        }
    }

    fn chars(s: &str) -> usize {
        s.chars().count()
    }

    /// The parts without their "часть N из M" headers, glued back together.
    fn bodies(parts: &[String]) -> String {
        if parts.len() == 1 {
            return parts[0].clone();
        }
        parts
            .iter()
            .map(|p| p.split_once("\n\n").expect("part header").1)
            .collect()
    }

    #[test]
    fn bootstrap_order_rules_system_tools_history() {
        let req = request(
            vec![
                msg("system", "SYSTEM-PROMPT-MARK"),
                msg("user", "USER-TASK-MARK"),
            ],
            vec![read_tool()],
        );
        let parts = bootstrap(&req, &[]);
        assert_eq!(parts.len(), 1);
        let t = &parts[0];
        assert!(!gateway_rules().is_empty());
        assert!(t.starts_with(gateway_rules()), "{}", t);
        let sys = t.find("SYSTEM-PROMPT-MARK").unwrap();
        let tools = t.find("### read").unwrap();
        let user = t.find("USER-TASK-MARK").unwrap();
        assert!(
            gateway_rules().len() <= sys && sys < tools && tools < user,
            "{}",
            t
        );
        assert!(t.ends_with(REMINDER), "{}", t);
    }

    #[test]
    fn execute_dropped_unless_allowed() {
        let tools = vec![
            read_tool(),
            tool(
                "execute",
                json!({"type": "object", "properties": {"code": {"type": "string"}}}),
            ),
        ];
        let hidden = compact_tools(&tools, &[]);
        assert!(hidden.contains("### read"), "{}", hidden);
        assert!(!hidden.contains("execute"), "{}", hidden);
        let shown = compact_tools(&tools, &["execute".to_string()]);
        assert!(shown.contains("### execute"), "{}", shown);

        let req = request(vec![msg("user", "hi")], tools);
        assert!(!bootstrap(&req, &[])[0].contains("### execute"));
        assert!(bootstrap(&req, &["execute".to_string()])[0].contains("### execute"));
    }

    #[test]
    fn compact_schema_keeps_names_types_required_enums_only() {
        let t = tool(
            "fetch",
            json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "additionalProperties": false,
                "required": ["url"],
                "properties": {
                    "format": {"type": "string", "enum": ["text", "markdown", "html"], "description": "Output format"},
                    "url": {"type": "string", "pattern": "^https?://", "description": "The URL\nto fetch"},
                    "timeout": {"type": "number", "exclusiveMinimum": 0, "maximum": 120, "default": 30},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "questions": {"type": "array", "items": {
                        "type": "object",
                        "required": ["question"],
                        "properties": {
                            "question": {"type": "string", "description": "Complete question"},
                            "options": {"type": "array", "items": {"type": "object", "properties": {"label": {"type": "string"}}}}
                        }
                    }},
                    "mode": {"type": ["string", "null"]}
                }
            }),
        );
        let out = compact_tools(
            &[t, tool("ping", json!({"type": "object", "properties": {}}))],
            &[],
        );
        assert_eq!(
            out,
            "### fetch\n\
             fetch tool\n\
             Параметры:\n\
             - url: string [required] — The URL to fetch\n\
             - format: string [enum: text|markdown|html] — Output format\n\
             - mode: string|null\n\
             - questions: array<{question: string, options?: array<object>}>\n\
             - tags: array<string>\n\
             - timeout: number\n\
             \n\
             ### ping\n\
             ping tool\n\
             Параметры: нет"
        );
    }

    #[test]
    fn bootstrap_from_real_capture_fits_one_part() {
        let req: ChatRequest =
            serde_json::from_str(include_str!("../tests/fixtures/opencode-first.json")).unwrap();
        let parts = bootstrap(&req, &[]);
        assert_eq!(
            parts.len(),
            1,
            "sizes: {:?}",
            parts.iter().map(|p| chars(p)).collect::<Vec<_>>()
        );
        let t = &parts[0];
        assert!(chars(t) <= MAX_MESSAGE_CHARS);
        assert!(t.starts_with(gateway_rules()));
        assert!(t.contains("Working directory: /home/user/project"));
        for name in [
            "glob",
            "grep",
            "patch",
            "question",
            "read",
            "shell",
            "skill",
            "subagent",
            "webfetch",
            "websearch",
        ] {
            assert!(
                t.contains(&format!("### {}\n", name)),
                "tool {} missing",
                name
            );
        }
        assert!(!t.contains("### execute"));
        assert!(t.contains("- command: string [required]"));
        assert!(t.contains("Прочитай a.txt и скажи, что в нём"));
    }

    #[test]
    fn bootstrap_compresses_old_tool_results_to_2000_and_keeps_last_3() {
        let letters = ['Ж', 'Щ', 'Ю', 'Я', 'Ф'];
        let mut messages = vec![msg("system", "sys"), msg("user", "task")];
        for (i, l) in letters.iter().enumerate() {
            let id = format!("call_{}", i);
            messages.push(calls(&[(&id, "read", "{\"path\":\"a.txt\"}")]));
            messages.push(result(&id, &l.to_string().repeat(10_000)));
        }
        messages.push(msg("user", "next"));
        let parts = bootstrap(&request(messages, vec![read_tool()]), &[]);
        assert_eq!(parts.len(), 1);
        let t = &parts[0];
        for l in &letters[..2] {
            assert!(
                t.contains(&l.to_string().repeat(2_000)),
                "old result {} cut too short",
                l
            );
            assert!(
                !t.contains(&l.to_string().repeat(2_001)),
                "old result {} not cut",
                l
            );
        }
        for l in &letters[2..] {
            assert!(
                t.contains(&l.to_string().repeat(10_000)),
                "recent result {} cut",
                l
            );
        }
        assert_eq!(t.matches("[обрезано").count(), 2, "{}", t);
        assert!(t.contains("[результат call_4 · read]"));
        assert!(t.contains(
            "```tool_calls\n[{\"name\": \"read\", \"arguments\": {\"path\":\"a.txt\"}}]\n```"
        ));
    }

    /// The task, then `steps` assistant steps (a call and a result of
    /// `result_chars`), a second user message in the middle.
    fn agent_loop(steps: usize, result_chars: usize) -> Vec<Message> {
        let mut messages = vec![msg("system", "sys"), msg("user", "TASK-FIRST")];
        for i in 0..steps {
            if i == steps / 2 {
                messages.push(msg("user", "USER-MIDDLE"));
            }
            let id = format!("c{:03}", i);
            let mut a = calls(&[(&id, "read", "{}")]);
            a.content = Some(Content::Text(format!("S{:03}", i)));
            messages.push(a);
            messages.push(result(&id, &"р".repeat(result_chars)));
        }
        messages
    }

    #[test]
    fn bootstrap_drops_oldest_turns_with_note_when_over_two_parts() {
        // Old results are cut to OLD_RESULT_CHARS, so about this many steps
        // are ~1.4 times the two-part budget (80 steps at 60_000).
        let steps = BOOTSTRAP_BUDGET / OLD_RESULT_CHARS * 4 / 3;
        assert!(steps < 1_000, "step labels have three digits");
        let parts = bootstrap(&request(agent_loop(steps, 5_000), vec![read_tool()]), &[]);
        assert!(parts.len() <= 2, "{} parts", parts.len());
        assert!(parts.iter().all(|p| chars(p) <= MAX_MESSAGE_CHARS));
        let t = bodies(&parts);
        let kept = (0..steps)
            .filter(|i| t.contains(&format!("S{:03}", i)))
            .count();
        let dropped = steps - kept;
        assert!(dropped >= 1);
        assert!(kept >= steps * 9 / 16, "only {} steps kept", kept);
        for i in 0..steps {
            assert_eq!(
                t.contains(&format!("S{:03}", i)),
                i >= dropped,
                "step {}",
                i
            );
        }
        // User messages are never dropped, the original task included.
        let task = t.find("[пользователь]\nTASK-FIRST").unwrap();
        let middle = t.find("[пользователь]\nUSER-MIDDLE").unwrap();
        let note = t
            .find(&format!("[{} ранних ходов опущено]", dropped))
            .expect("note");
        let first_kept = t.find(&format!("S{:03}", dropped)).unwrap();
        assert!(
            task < note && note < first_kept && first_kept < middle,
            "{}",
            t
        );
        assert_eq!(t.matches("ранних ходов опущено").count(), 1);
    }

    #[test]
    fn bootstrap_caps_latest_tool_results_when_the_latest_turn_alone_is_too_big() {
        let mut messages = agent_loop(6, 300);
        messages.push(calls(&[
            ("c1", "read", "{}"),
            ("c2", "read", "{}"),
            ("c3", "read", "{}"),
        ]));
        messages.push(result("c1", "short result"));
        // Each is 2.5 messages long: the latest turn alone is over the budget.
        let big = MAX_MESSAGE_CHARS * 5 / 2;
        messages.push(result("c2", &"Б".repeat(big)));
        messages.push(result("c3", &"В".repeat(big)));
        let parts = bootstrap(&request(messages, vec![read_tool()]), &[]);
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| chars(p) <= MAX_MESSAGE_CHARS));
        let t = bodies(&parts);
        assert!(t.contains("short result"));
        assert!(
            t.contains(&"Б".repeat(MAX_MESSAGE_CHARS * 2 / 3))
                && t.contains(&"В".repeat(MAX_MESSAGE_CHARS * 2 / 3))
        );
        assert_eq!(t.matches("[обрезано").count(), 2);
        // The earlier history stays: the results were cut instead.
        assert!(!t.contains("ранних ходов опущено"), "{}", t);
        for i in 0..6 {
            assert!(t.contains(&format!("S{:03}", i)), "step {}", i);
        }
        assert!(t.contains("TASK-FIRST") && t.contains("USER-MIDDLE"));
    }

    #[test]
    fn bootstrap_latest_results_keep_2000_each_then_older_steps_go() {
        let mut messages = agent_loop(10, 300);
        // The user message alone leaves ~5_800 chars of the budget.
        let user_chars = BOOTSTRAP_BUDGET - 5_800;
        messages.push(msg("user", &"п".repeat(user_chars)));
        messages.push(calls(&[("x1", "read", "{}")]));
        messages.push(result("x1", &"Ё".repeat(50_000)));
        let parts = bootstrap(&request(messages, vec![read_tool()]), &[]);
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| chars(p) <= MAX_MESSAGE_CHARS));
        let t = bodies(&parts);
        assert!(t.contains(&"Ё".repeat(2_000)) && !t.contains(&"Ё".repeat(2_001)));
        let kept = (0..10)
            .filter(|i| t.contains(&format!("S{:03}", i)))
            .count();
        assert!(kept < 10, "older steps dropped");
        assert!(t.contains(&format!("[{} ранних ходов опущено]", 10 - kept)));
        assert!(t.contains("TASK-FIRST") && t.contains(&"п".repeat(user_chars)));
    }

    #[test]
    fn bootstrap_over_two_parts_only_when_user_messages_alone_are_too_big() {
        let mut messages = vec![msg("system", "sys")];
        for i in 0..20 {
            // Together the user messages are about three parts long.
            let each = 3 * PART_BODY / 20;
            messages.push(msg("user", &format!("U{:02} {}", i, "у".repeat(each))));
            messages.push(msg("assistant", &format!("A{:02}", i)));
        }
        let parts = bootstrap(&request(messages, vec![read_tool()]), &[]);
        assert!(parts.len() > 2, "{} parts", parts.len());
        assert!(parts.iter().all(|p| chars(p) <= MAX_MESSAGE_CHARS));
        let t = bodies(&parts);
        for i in 0..20 {
            assert!(
                t.contains(&format!("U{:02} ", i)),
                "user message {} dropped",
                i
            );
        }
        assert!(t.contains("A19"), "the latest step stays");
    }

    #[test]
    fn unparsable_stored_arguments_are_not_shown_as_a_string() {
        let req = request(
            vec![
                msg("user", "go"),
                calls(&[("c1", "read", "{\"path\": broken")]),
                result("c1", "err"),
            ],
            vec![read_tool()],
        );
        let t = &bootstrap(&req, &[])[0];
        assert!(
            t.contains("[{\"name\": \"read\", \"arguments\": {} /* аргументы не разобраны */}]"),
            "{}",
            t
        );
        assert!(!t.contains("broken"), "{}", t);
    }

    #[test]
    fn delta_formats_tool_results_with_id_and_name() {
        let msgs = vec![
            calls(&[("call_1", "read", "{\"path\":\"a.txt\"}")]),
            result("call_1", "Read file a.txt, lines 1-1\n1: alpha"),
            msg("user", "и что дальше?"),
        ];
        let parts = delta(&msgs, false, None);
        assert_eq!(parts.len(), 1);
        let t = &parts[0];
        assert!(
            t.starts_with("[результат call_1 · read]\nRead file a.txt, lines 1-1\n1: alpha"),
            "{}",
            t
        );
        let user = t.find("[пользователь]\nи что дальше?").unwrap();
        assert!(user > 0);
        assert!(
            !t.contains("[ассистент]"),
            "own calls are not sent back: {}",
            t
        );
        assert!(!REMINDER.is_empty() && !REMINDER.contains('\n'));
        assert!(t.ends_with(&format!("\n\n{}", REMINDER)), "{}", t);
        assert!(!t.contains(INTERRUPTED_NOTE) || INTERRUPTED_NOTE.is_empty());
    }

    #[test]
    fn delta_starts_with_interrupted_note_when_flagged() {
        let msgs = vec![msg("user", "ещё раз")];
        assert!(!INTERRUPTED_NOTE.is_empty());
        let t = &delta(&msgs, true, None)[0];
        assert!(t.starts_with(INTERRUPTED_NOTE), "{}", t);
        assert!(t.contains("[пользователь]\nещё раз"));
        assert!(!delta(&msgs, false, None)[0].contains(INTERRUPTED_NOTE));
    }

    #[test]
    fn delta_includes_updated_rules_block_when_given() {
        let msgs = vec![calls(&[("c9", "glob", "{}")]), result("c9", "a.txt")];
        let tools = compact_tools(&[read_tool()], &[]);
        let t = &delta(&msgs, true, Some(("NEW-SYSTEM-PROMPT", &tools)))[0];
        let note = t.find(INTERRUPTED_NOTE).unwrap();
        let block = t.find("Правила и инструменты обновлены").unwrap();
        let sys = t.find("NEW-SYSTEM-PROMPT").unwrap();
        let tl = t.find(&tools).unwrap();
        let res = t.find("[результат c9 · glob]\na.txt").unwrap();
        assert!(note < block && block < sys && sys < tl && tl < res, "{}", t);
        assert!(!delta(&msgs, false, None)[0].contains("Правила и инструменты обновлены"));
    }

    #[test]
    fn everything_redacted_before_split() {
        let text = format!(
            "{} AKIAABCDEFGHIJKLMNOP {} AKIAQRSTUVWXYZ234567 {}",
            "x".repeat(PART_BODY - 10),
            "y".repeat(80),
            "z".repeat(30_000)
        );
        let parts = split(&text);
        assert_eq!(parts.len(), 2);
        for p in &parts {
            assert!(chars(p) <= MAX_MESSAGE_CHARS);
            assert!(!p.contains("AKIA"), "key leaked into a part");
            assert!(!p.contains("ABCDEFGHIJKLMNOP") && !p.contains("QRSTUVWXYZ234567"));
        }
        assert_eq!(bodies(&parts), redact(&text, "").stdout);
        assert_eq!(
            bodies(&parts)
                .matches("«redacted:aws_access_key_id»")
                .count(),
            2
        );

        // Short text stays one part with no header, still redacted.
        assert_eq!(
            split("key AKIAABCDEFGHIJKLMNOP here"),
            vec!["key «redacted:aws_access_key_id» here".to_string()]
        );
    }

    #[test]
    fn delta_with_huge_results_is_cut_like_the_bootstrap() {
        let big = MAX_MESSAGE_CHARS * 5 / 2;
        let msgs = vec![
            calls(&[
                ("c1", "read", "{}"),
                ("c2", "read", "{}"),
                ("c3", "read", "{}"),
            ]),
            result("c1", "short result"),
            result("c2", &"Б".repeat(big)),
            result("c3", &"В".repeat(big)),
            msg("user", "USER-NOTE"),
        ];
        let parts = delta(&msgs, false, None);
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| chars(p) <= MAX_MESSAGE_CHARS));
        let t = bodies(&parts);
        assert!(t.contains("[результат c1 · read]\nshort result"));
        assert!(
            t.contains(&"Б".repeat(MAX_MESSAGE_CHARS * 2 / 3))
                && t.contains(&"В".repeat(MAX_MESSAGE_CHARS * 2 / 3))
        );
        assert_eq!(t.matches("[обрезано").count(), 2);
        assert!(t.contains("[пользователь]\nUSER-NOTE"));
        assert!(t.ends_with(REMINDER));

        // A delta within the budget is not cut.
        let msgs = vec![
            calls(&[("c4", "read", "{}")]),
            result("c4", &"Г".repeat(MAX_MESSAGE_CHARS)),
        ];
        let t = bodies(&delta(&msgs, false, None));
        assert!(t.contains(&"Г".repeat(MAX_MESSAGE_CHARS)) && !t.contains("[обрезано"));
    }

    #[test]
    fn huge_tool_result_split_parts_within_limit() {
        let mut huge = String::new();
        let mut i = 0;
        let mut planted = false;
        while chars(&huge) < MAX_MESSAGE_CHARS * 10 / 3 {
            huge.push_str(&format!("строка {}: данные 🚀 ü\n", i));
            // The token sits in the second part.
            if !planted && chars(&huge) > MAX_MESSAGE_CHARS * 5 / 4 {
                huge.push_str("token ghp_abcdefghijklmnopqrstuvwxyz0123456789AB\n");
                planted = true;
            }
            i += 1;
        }
        // A whole message as `delta` would build it, were its result not cut.
        let text = format!("[результат call_7 · read]\n{}\n\n{}", huge, REMINDER);
        let parts = split(&text);
        let m = parts.len();
        assert!(m >= 4, "{} parts", m);
        for (n, p) in parts.iter().enumerate() {
            assert!(
                chars(p) <= MAX_MESSAGE_CHARS,
                "part {} has {} chars",
                n + 1,
                chars(p)
            );
            assert!(!p.contains("ghp_"));
            if n + 1 < m {
                assert!(p.starts_with(&format!("часть {} из {}, ответь словом ok\n\n", n + 1, m)));
                assert!(p.ends_with('\n'), "part {} not cut at a line end", n + 1);
            }
        }
        assert!(parts[m - 1].starts_with(&format!("часть {} из {}", m, m)));
        assert!(parts[m - 1].ends_with(REMINDER));
        let expected = format!("[результат call_7 · read]\n{}", redact(&huge, "").stdout);
        assert!(bodies(&parts).starts_with(&expected));
    }

    #[test]
    fn the_continued_note_says_the_parts_above_still_count() {
        assert_eq!(
            continued_note(2),
            "Продолжение сообщения выше: отправка прервалась после части 2, полученные части \
             остаются в силе. Ниже — остальное с части 3 (она повторяется с начала); ответь на \
             сообщение целиком после последней части."
        );
        assert!(!continued_note(1).contains("не нужен"));
    }

    #[test]
    fn the_rest_of_a_message_numbers_its_parts_on() {
        // Even a short rest gets the header: it is the last part.
        let short = split_after("остаток", 1);
        assert_eq!(
            short,
            vec![(
                "часть 2 из 2, последняя\n\nостаток".to_string(),
                "остаток".len()
            )]
        );
        // A rest of two parts after two answered ones: parts 3 and 4 of 4.
        let text = "строка данных\n".repeat(MAX_MESSAGE_CHARS * 3 / 2 / 14);
        let parts = split_after(&text, 2);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].0.starts_with("часть 3 из 4, ответь словом ok\n\n"));
        assert!(parts[1].0.starts_with("часть 4 из 4, последняя\n\n"));
        assert!(parts.iter().all(|(p, _)| chars(p) <= MAX_MESSAGE_CHARS));
        assert_eq!(parts[1].1, text.len());
        let pieces: String = parts
            .iter()
            .map(|(p, _)| p.split_once("\n\n").unwrap().1)
            .collect();
        assert_eq!(pieces, text);
        assert_eq!(
            &text[..parts[0].1],
            parts[0].0.split_once("\n\n").unwrap().1
        );
        // Nothing answered: as before.
        assert_eq!(split_after("коротко", 0), split_redacted("коротко"));
    }
}
