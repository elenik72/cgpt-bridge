//! `local-title`: OpenCode names a session with a model call. The gateway
//! answers it locally, without ChatGPT, with the first words of the message.

use crate::openai::ChatRequest;

const MAX_WORDS: usize = 8;
const QUOTES: &[char] = &['"', '\'', '`', '«', '»', '“', '”', '„', '‘', '’'];
const FALLBACK: &str = "Новый чат";

/// The first 6-8 words of what is to be named: the text inside the first
/// `<message>…</message>` of the last user message if there is one (that is
/// how OpenCode wraps it), otherwise the first user message. Quotes are
/// stripped.
pub fn local_title(req: &ChatRequest) -> String {
    let wrapped = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| message_block(&m.text()));
    let source = wrapped.or_else(|| {
        req.messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.text())
    });
    let words: Vec<&str> = source
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(|w| w.trim_matches(QUOTES))
        .filter(|w| !w.is_empty())
        .take(MAX_WORDS)
        .collect();
    let title = words.join(" ");
    let title = title.trim_end_matches(['.', ',', ':', ';', '!', '?', '…']);
    if title.is_empty() {
        FALLBACK.to_string()
    } else {
        title.to_string()
    }
}

fn message_block(text: &str) -> Option<String> {
    let start = text.find("<message>")? + "<message>".len();
    let len = text[start..].find("</message>")?;
    Some(text[start..start + len].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{Content, Message};

    fn req(messages: Vec<(&str, &str)>) -> ChatRequest {
        ChatRequest {
            model: "local-title".into(),
            messages: messages
                .into_iter()
                .map(|(role, text)| Message {
                    role: role.into(),
                    content: Some(Content::Text(text.into())),
                    ..Default::default()
                })
                .collect(),
            tools: None,
            stream: None,
        }
    }

    #[test]
    fn takes_the_first_message_block_and_strips_quotes() {
        let r = req(vec![
            ("system", "You name conversations"),
            (
                "user",
                "Here is the first message:\n\n<message>\n\"Прочитай a.txt и скажи, что в нём\"\n</message>\n\nReply with only the title.",
            ),
        ]);
        assert_eq!(local_title(&r), "Прочитай a.txt и скажи, что в нём");
    }

    #[test]
    fn caps_at_eight_words_and_drops_trailing_punctuation() {
        let r = req(vec![(
            "user",
            "one two three four five six seven eight nine ten",
        )]);
        assert_eq!(local_title(&r), "one two three four five six seven eight");
        let r = req(vec![("user", "Fix the build, please!")]);
        assert_eq!(local_title(&r), "Fix the build, please");
    }

    #[test]
    fn falls_back_to_the_first_user_message_without_a_block() {
        let r = req(vec![
            ("system", "s"),
            ("user", "«Привет 🎉 мир»"),
            ("assistant", "x"),
        ]);
        assert_eq!(local_title(&r), "Привет 🎉 мир");
    }

    #[test]
    fn empty_input_gets_a_fallback() {
        assert_eq!(local_title(&req(vec![])), "Новый чат");
        assert_eq!(local_title(&req(vec![("user", "  \"\"  ")])), "Новый чат");
    }
}
