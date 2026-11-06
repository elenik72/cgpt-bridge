//! OpenAI chat-completions types: what OpenCode sends and what the gateway
//! answers. Only the fields the gateway reads are modelled; unknown request
//! fields (`stream_options`, `max_completion_tokens`, ...) are ignored.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize, Clone)]
pub struct ChatRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Message {
    pub role: String,
    /// `null` on assistant messages that only carry tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Message content: a plain string, or a list of typed parts.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<Part>),
}

/// One part of a message's content. Besides text, OpenCode (2.2.5,
/// `packages/ai/src/protocols/openai-chat.ts`) sends an image as
/// `{"type":"image_url","image_url":{"url":"data:image/png;base64,…"}}` (or
/// an http(s) URL) and a PDF as `{"type":"file","file":{"filename":…,
/// "file_data":"data:application/pdf;base64,…"}}`; images a tool read come
/// as a user message of their own after the tool results. `image_url` and
/// `file` are kept as JSON: some clients send `image_url` as a bare string.
/// What the gateway makes of them is `images`'s.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Part {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    /// A JSON document in a string, as OpenAI defines it.
    pub arguments: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Tool {
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Value,
}

fn function_kind() -> String {
    "function".to_string()
}

impl Message {
    /// The text of the message: the string itself, or the text parts joined
    /// with newlines (parts without text, such as images, are skipped).
    pub fn text(&self) -> String {
        match &self.content {
            None => String::new(),
            Some(Content::Text(s)) => s.clone(),
            Some(Content::Parts(parts)) => parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl ChatRequest {
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// Characters in all message texts and tool-call arguments; the basis of
    /// the `usage` estimate.
    pub fn prompt_chars(&self) -> usize {
        self.messages
            .iter()
            .map(|m| {
                let calls: usize = m
                    .tool_calls
                    .iter()
                    .flatten()
                    .map(|c| c.function.arguments.chars().count())
                    .sum();
                m.text().chars().count() + calls
            })
            .sum()
    }
}

/// What a turn produced for OpenCode.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Text(String),
    ToolCalls {
        /// Text that accompanied the calls; may be empty.
        content: String,
        calls: Vec<ToolCall>,
    },
}

impl Reply {
    pub fn completion_chars(&self) -> usize {
        match self {
            Reply::Text(t) => t.chars().count(),
            Reply::ToolCalls { content, calls } => {
                content.chars().count()
                    + calls
                        .iter()
                        .map(|c| {
                            c.function.name.chars().count() + c.function.arguments.chars().count()
                        })
                        .sum::<usize>()
            }
        }
    }

    pub fn finish_reason(&self) -> &'static str {
        match self {
            Reply::Text(_) => "stop",
            Reply::ToolCalls { .. } => "tool_calls",
        }
    }
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    /// The gateway has no tokenizer: tokens are characters / 4.
    pub fn estimate(prompt_chars: usize, completion_chars: usize) -> Usage {
        let prompt_tokens = (prompt_chars / 4) as u64;
        let completion_tokens = (completion_chars / 4) as u64;
        Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn chunk_json(id: &str, model: &str, delta: Value, finish: Value) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": unix_seconds(),
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    })
}

pub fn sse_line(value: &Value) -> String {
    format!("data: {}\n\n", value)
}

/// The same answer as one non-streamed `chat.completion` object.
pub fn completion_json(id: &str, model: &str, reply: &Reply, usage: Usage) -> Value {
    let message = match reply {
        Reply::Text(t) => json!({ "role": "assistant", "content": t }),
        Reply::ToolCalls { content, calls } => {
            let calls: Vec<Value> = calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.function.name, "arguments": c.function.arguments },
                    })
                })
                .collect();
            let content = if content.is_empty() {
                Value::Null
            } else {
                json!(content)
            };
            json!({ "role": "assistant", "content": content, "tool_calls": calls })
        }
    };
    json!({
        "id": id,
        "object": "chat.completion",
        "created": unix_seconds(),
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": reply.finish_reason() }],
        "usage": usage,
    })
}

/// OpenAI-style error body.
pub fn error_json(code: &str, message: &str) -> Value {
    json!({ "error": { "message": message, "type": "cgpt_gateway", "code": code } })
}
