//! Images from OpenCode into ChatGPT.
//!
//! A turn's text goes to ChatGPT redacted, as before. The images of the
//! messages the turn sends (its delta, or the whole history in a bootstrap
//! pack) go as files of the ask that carries the message's last part: the
//! extension puts them into the composer before it types the text. In the
//! text every image becomes a line `[изображение N во вложении]`, or a note
//! saying why it was not sent. Images are not redacted (they cannot be): they
//! reach ChatGPT as they are. Each one sent is logged (type and size, never
//! the content).
//!
//! What is sent: PNG, JPEG, GIF and WebP from `data:` URLs (`image_url`
//! parts, and `file` parts holding an image), recognised by their first
//! bytes, not by the declared type. An http(s) URL is not fetched: the
//! gateway never connects anywhere but to the bridge, and a fetch would let a
//! request make it reach any address; the URL goes into the text as a note.
//! Other files (OpenCode sends a PDF as a `file` part) become a note too.
//!
//! Limits: the bridge carries an ask in one Native Messaging frame, and
//! Chrome takes at most 1 MB from the host, so the files of one ask are at
//! most [`MAX_ASK_IMAGE_BYTES`] (about 930 KB as base64), at most
//! [`MAX_IMAGES`] of them. Images go newest first while they fit; every other
//! one becomes a note in its place, such as
//! `[изображение не отправлено: 2,1 МБ, предел 700 КБ на сообщение]`, and a
//! log line. The turn itself never fails for an image: one over a limit stays
//! in OpenCode's history, and a refusal would refuse every later turn of the
//! session. The part that carries the files keeps its text within the room
//! the files leave in the frame (`prompt::split_after_room`).
//!
//! Without an extension that takes files (`status_result` without
//! `attachments: true`) every image becomes the note
//! `[изображение не отправлено: расширение устарело]`, and the log says so.

use cgpt_bridge_protocol::{Attachment, MAX_FRAME_BYTES};
use serde_json::Value;

use crate::log::log;
use crate::openai::{ChatRequest, Content, Message, Part};

/// Most bytes of one image.
pub const MAX_IMAGE_BYTES: usize = 700 * 1024;
/// Most bytes of the images of one ask together.
pub const MAX_ASK_IMAGE_BYTES: usize = 700 * 1024;
/// Most images in one ask.
pub const MAX_IMAGES: usize = 10;
/// Room in a frame for everything of an `ask` but its text and its files
/// (the type, id, timeout, target URL, project name, `capture_stream` and
/// JSON punctuation).
const FRAME_RESERVE: usize = 16 * 1024;
/// The JSON of one attachment besides its base64: the field names and our
/// own file name (`image-10.jpeg`) and type.
const ATTACHMENT_OVERHEAD: usize = 128;
/// The least room the files leave for the text of the part that carries
/// them, as JSON.
pub const MIN_TEXT_ROOM: usize = 48 * 1024;

const fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

const _: () = assert!(
    base64_len(MAX_ASK_IMAGE_BYTES) + MAX_IMAGES * ATTACHMENT_OVERHEAD + FRAME_RESERVE + MIN_TEXT_ROOM
        <= MAX_FRAME_BYTES as usize,
    "the images of one ask would leave too little room in a bridge frame"
);

/// The note for an image an extension that predates files would have lost.
pub const OLD_EXTENSION_NOTE: &str = "[изображение не отправлено: расширение устарело]";

/// What a request becomes for one turn's text: its image parts replaced by
/// markers and notes, and the files to send.
pub struct Prepared {
    /// The request with every non-text part of its messages replaced: by a
    /// text marker or note in the messages the turn sends, by nothing in the
    /// others. Message order and count are the request's.
    pub req: ChatRequest,
    files: Vec<Attachment>,
    /// Log lines for the images not sent (no content).
    notes: Vec<String>,
}

impl Prepared {
    /// The files to send with the turn's last part; logs the images that are
    /// not sent. Called once, for the plan the turn goes with.
    pub fn take_files(self) -> (ChatRequest, Vec<Attachment>) {
        for line in &self.notes {
            log(line);
        }
        (self.req, self.files)
    }
}

/// One non-text part of a message the turn sends.
enum Found {
    Image {
        mime: &'static str,
        ext: &'static str,
        bytes: Vec<u8>,
    },
    /// Not an image the gateway can send: the note for ChatGPT and the log
    /// line (no content, no URL).
    Note { text: String, log: String },
}

/// The images of the messages a turn sends, and the request's text with
/// them replaced. `sent_from` is the first history message (the request's
/// non-system messages, as `session::history` counts them) whose text the
/// turn sends. Images go newest first while they fit (each and all together
/// at most [`MAX_ASK_IMAGE_BYTES`], at most [`MAX_IMAGES`]); every other one
/// becomes a note in its place. Never a failure: an image over a limit stays
/// in OpenCode's history, and refusing it would refuse every later turn of
/// the session. The choice depends only on the messages sent, so a delta
/// sent again makes the same notes. `takes_files`: the extension said it
/// puts files into the composer.
pub fn prepare(req: &ChatRequest, sent_from: usize, takes_files: bool) -> Prepared {
    // (message index in `req.messages`, part index, history index, found)
    let mut found: Vec<(usize, usize, usize, Found)> = Vec::new();
    let mut h = 0;
    for (i, m) in req.messages.iter().enumerate() {
        if m.role == "system" {
            continue;
        }
        if h >= sent_from {
            if let Some(Content::Parts(parts)) = &m.content {
                for (j, p) in parts.iter().enumerate() {
                    if let Some(f) = read_part(p) {
                        found.push((i, j, h, f));
                    }
                }
            }
        }
        h += 1;
    }

    // Which images go: newest first, while they fit; why each other one
    // does not.
    let mut chosen: Vec<Result<(), Skipped>> = vec![Ok(()); found.len()];
    let mut count = 0;
    let mut total = 0;
    for k in (0..found.len()).rev() {
        let Found::Image { bytes, .. } = &found[k].3 else {
            continue;
        };
        let len = bytes.len();
        chosen[k] = if len > MAX_IMAGE_BYTES {
            Err(Skipped::Large)
        } else if count >= MAX_IMAGES {
            Err(Skipped::Many)
        } else if total + len > MAX_ASK_IMAGE_BYTES {
            Err(Skipped::Together)
        } else {
            count += 1;
            total += len;
            Ok(())
        };
    }

    // Number the chosen ones in the order the text shows them.
    let mut replaced: Vec<Vec<(usize, String)>> = vec![Vec::new(); req.messages.len()];
    let mut files = Vec::new();
    let mut notes = Vec::new();
    for (k, (i, j, _, f)) in found.into_iter().enumerate() {
        let text = match f {
            Found::Image { mime, bytes, .. } if !takes_files => {
                notes.push(format!(
                    "cgpt-gateway: image not sent (extension too old): {} {} bytes",
                    mime,
                    bytes.len()
                ));
                OLD_EXTENSION_NOTE.to_string()
            }
            Found::Image { mime, ext, bytes } if chosen[k].is_ok() => {
                let n = files.len() + 1;
                files.push(Attachment {
                    name: format!("image-{}.{}", n, ext),
                    mime: mime.to_string(),
                    data_base64: encode_base64(&bytes),
                });
                format!("[изображение {} во вложении]", n)
            }
            Found::Image { mime, bytes, .. } => {
                let why = chosen[k].err().unwrap_or(Skipped::Together);
                notes.push(format!(
                    "cgpt-gateway: image not sent ({}): {} {} bytes",
                    why.log(),
                    mime,
                    bytes.len()
                ));
                why.note(bytes.len())
            }
            Found::Note { text, log } => {
                notes.push(log);
                text
            }
        };
        replaced[i].push((j, text));
    }

    let messages = req
        .messages
        .iter()
        .zip(replaced)
        .map(|(m, parts)| shown_message(m, &parts))
        .collect();
    Prepared {
        req: ChatRequest {
            messages,
            ..without_messages(req)
        },
        files,
        notes,
    }
}

/// Why an image is not sent.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Skipped {
    /// Over the limit by itself.
    Large,
    /// Newer images already take [`MAX_IMAGES`].
    Many,
    /// It would take the newer ones over [`MAX_ASK_IMAGE_BYTES`].
    Together,
}

impl Skipped {
    /// The note in the text in the image's place.
    fn note(self, len: usize) -> String {
        match self {
            Skipped::Large => format!(
                "[изображение не отправлено: {}, предел {} на сообщение]",
                size(len),
                size(MAX_ASK_IMAGE_BYTES)
            ),
            Skipped::Many => format!(
                "[изображение не отправлено: больше {} изображений в одном сообщении]",
                MAX_IMAGES
            ),
            Skipped::Together => format!(
                "[изображение не отправлено: {}, вместе с более новыми больше предела {} на \
                 сообщение]",
                size(len),
                size(MAX_ASK_IMAGE_BYTES)
            ),
        }
    }

    fn log(self) -> &'static str {
        match self {
            Skipped::Large => "over the limit by itself",
            Skipped::Many => "too many images",
            Skipped::Together => "over the limit with newer images",
        }
    }
}

/// `n` bytes for a person: KB below a megabyte, MB with one decimal (a
/// comma) above.
fn size(n: usize) -> String {
    if n < 1024 * 1024 {
        format!("{} КБ", n.div_ceil(1024))
    } else {
        format!("{:.1} МБ", n as f64 / (1024.0 * 1024.0)).replace('.', ",")
    }
}

/// The request without its messages (they are rebuilt, not cloned: they may
/// hold megabytes of base64).
fn without_messages(req: &ChatRequest) -> ChatRequest {
    ChatRequest {
        model: req.model.clone(),
        messages: Vec::new(),
        tools: req.tools.clone(),
        stream: req.stream,
    }
}

/// `m` with the parts in `replaced` turned into text parts, and every other
/// non-text part (of a message the turn does not send) dropped. Text parts
/// stay as they are, so `text()` reads the same with the markers between.
fn shown_message(m: &Message, replaced: &[(usize, String)]) -> Message {
    let content = match &m.content {
        Some(Content::Parts(parts)) => Some(Content::Parts(
            parts
                .iter()
                .enumerate()
                .filter_map(|(j, p)| {
                    if let Some((_, text)) = replaced.iter().find(|(k, _)| *k == j) {
                        return Some(Part {
                            kind: "text".into(),
                            text: Some(text.clone()),
                            ..Default::default()
                        });
                    }
                    p.text.as_ref().map(|t| Part {
                        kind: p.kind.clone(),
                        text: Some(t.clone()),
                        ..Default::default()
                    })
                })
                .collect(),
        )),
        other => other.clone(),
    };
    Message {
        role: m.role.clone(),
        content,
        tool_calls: m.tool_calls.clone(),
        tool_call_id: m.tool_call_id.clone(),
    }
}

/// The room, in bytes of JSON, that `files` leave in a bridge frame for the
/// text of the ask that carries them; unlimited without files.
pub fn text_room(files: &[Attachment]) -> usize {
    if files.is_empty() {
        return usize::MAX;
    }
    let files_len: usize = files
        .iter()
        .map(|a| a.data_base64.len() + a.name.len() + a.mime.len() + 48)
        .sum();
    (MAX_FRAME_BYTES as usize).saturating_sub(FRAME_RESERVE + files_len)
}

/// One log line per file of the ask `ask_id`: its type and size.
pub fn log_sent(ask_id: &str, files: &[Attachment]) {
    for (n, a) in files.iter().enumerate() {
        log(format_args!(
            "cgpt-gateway: image {}/{} [{}] {} {} bytes (sent as it is, not redacted)",
            n + 1,
            files.len(),
            ask_id,
            a.mime,
            a.data_base64.len() / 4 * 3 - a.data_base64.chars().rev().take_while(|&c| c == '=').count()
        ));
    }
}

/// What a non-text part is; `None` for a text part.
fn read_part(p: &Part) -> Option<Found> {
    match p.kind.as_str() {
        "text" => None,
        "image_url" => {
            let url = match &p.image_url {
                Some(Value::String(u)) => Some(u.as_str()),
                Some(v) => v.get("url").and_then(Value::as_str),
                None => None,
            };
            Some(match url {
                Some(url) => read_url(url, None),
                None => unreadable("image_url without a url"),
            })
        }
        "file" => {
            let file = p.file.as_ref();
            let name = file
                .and_then(|f| f.get("filename"))
                .and_then(Value::as_str);
            let data = file.and_then(|f| f.get("file_data")).and_then(Value::as_str);
            Some(match data {
                Some(data) if data.starts_with("data:") => read_url(data, name),
                _ => Found::Note {
                    text: format!(
                        "[файл {} не отправлен: шлюз передаёт в ChatGPT только изображения]",
                        shown_name(name)
                    ),
                    log: "cgpt-gateway: file not sent (no inline data)".to_string(),
                },
            })
        }
        // A text-like part of another type keeps its text.
        _ if p.text.is_some() => None,
        other => Some(Found::Note {
            text: format!("[вложение типа {} не отправлено]", shown_kind(other)),
            log: format!(
                "cgpt-gateway: part of type {} not sent",
                shown_kind(other)
            ),
        }),
    }
}

/// An image (or file) at `url`. `name`: the file name a `file` part gave.
fn read_url(url: &str, name: Option<&str>) -> Found {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let shown: String = url.chars().take(300).collect();
        return Found::Note {
            text: format!(
                "[изображение по ссылке не отправлено: шлюз не загружает ссылки: {}]",
                shown
            ),
            log: "cgpt-gateway: image not sent (remote URL not fetched)".to_string(),
        };
    }
    let Some(rest) = url.strip_prefix("data:") else {
        return unreadable("not a data: or http(s) URL");
    };
    let Some((head, data)) = rest.split_once(',') else {
        return unreadable("data: URL without a comma");
    };
    let mut params = head.split(';');
    let declared = params.next().unwrap_or("").trim().to_ascii_lowercase();
    if !params.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        return unreadable("data: URL not in base64");
    }
    let Some(bytes) = decode_base64(data) else {
        return unreadable("bad base64");
    };
    match sniff(&bytes) {
        Some((mime, ext)) => Found::Image { mime, ext, bytes },
        None => {
            let kind = if declared.is_empty() {
                "без типа".to_string()
            } else {
                shown_kind(&declared)
            };
            let what = match name {
                Some(_) => format!("файл {}", shown_name(name)),
                None => "изображение".to_string(),
            };
            Found::Note {
                text: format!(
                    "[{} не отправлен{}: тип {} не поддерживается, шлюз передаёт PNG, JPEG, GIF \
                     и WebP]",
                    what,
                    if name.is_some() { "" } else { "о" },
                    kind
                ),
                log: format!(
                    "cgpt-gateway: attachment not sent (unsupported type {}): {} bytes",
                    shown_kind(&declared),
                    bytes.len()
                ),
            }
        }
    }
}

fn unreadable(why: &str) -> Found {
    Found::Note {
        text: "[изображение не отправлено: его данные не прочитаны]".to_string(),
        log: format!("cgpt-gateway: image not sent (unreadable: {})", why),
    }
}

/// A file name for the text: at most 100 characters, no control characters.
fn shown_name(name: Option<&str>) -> String {
    match name {
        Some(n) if !n.trim().is_empty() => n
            .chars()
            .filter(|c| !c.is_control())
            .take(100)
            .collect(),
        _ => "без имени".to_string(),
    }
}

/// A type name for the text and the log: what a MIME type or part type may
/// hold, at most 64 characters.
fn shown_kind(kind: &str) -> String {
    kind.chars()
        .filter(|c| c.is_ascii_alphanumeric() || "/+.-_".contains(*c))
        .take(64)
        .collect()
}

/// The image type by the first bytes: PNG, JPEG, GIF or WebP.
fn sniff(b: &[u8]) -> Option<(&'static str, &'static str)> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", "jpg"))
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(base64_len(bytes.len()));
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64 (standard or URL-safe alphabet; padding and ASCII whitespace
/// optional); `None` on anything else.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut padding = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        if padding > 0 {
            return None;
        }
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be the zero fill of a short last group.
    if bits >= 6 || acc != 0 || padding > 2 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::openai::{Content, Message, Part};
    use serde_json::json;

    /// A 1×1 PNG.
    pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";

    /// A PNG of `len` bytes (the signature, then zeros).
    pub fn png_of(len: usize) -> Vec<u8> {
        let mut b = PNG.to_vec();
        b.resize(len.max(PNG.len()), 0);
        b
    }

    pub fn data_url(mime: &str, bytes: &[u8]) -> String {
        format!("data:{};base64,{}", mime, encode_base64(bytes))
    }

    /// A user message: its text, then an image part per URL.
    pub fn user_with_images(text: &str, urls: &[String]) -> Message {
        let mut parts = vec![Part {
            kind: "text".into(),
            text: Some(text.into()),
            ..Default::default()
        }];
        for u in urls {
            parts.push(Part {
                kind: "image_url".into(),
                image_url: Some(json!({ "url": u })),
                ..Default::default()
            });
        }
        Message {
            role: "user".into(),
            content: Some(Content::Parts(parts)),
            ..Default::default()
        }
    }

    fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(Content::Text(text.into())),
            ..Default::default()
        }
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "chatgpt-web".into(),
            messages,
            tools: None,
            stream: None,
        }
    }

    #[test]
    fn base64_round_trips_and_rejects_garbage() {
        for len in 0..20 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let text = encode_base64(&bytes);
            assert_eq!(text.len(), base64_len(len));
            assert_eq!(decode_base64(&text), Some(bytes.clone()), "{text}");
            // Without padding and with line breaks too.
            let bare = text.trim_end_matches('=');
            assert_eq!(decode_base64(bare), Some(bytes.clone()));
            let wrapped: String = text
                .chars()
                .enumerate()
                .flat_map(|(i, c)| if i % 4 == 3 { vec![c, '\n'] } else { vec![c] })
                .collect();
            assert_eq!(decode_base64(&wrapped), Some(bytes));
        }
        assert_eq!(encode_base64(b"Man"), "TWFu");
        assert_eq!(encode_base64(b"Ma"), "TWE=");
        assert_eq!(encode_base64(b"M"), "TQ==");
        for bad in ["TQ=x", "T", "TR==", "a!bc", "TQ==="] {
            assert_eq!(decode_base64(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_type_is_read_from_the_bytes() {
        assert_eq!(sniff(PNG), Some(("image/png", "png")));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(("image/jpeg", "jpg")));
        assert_eq!(sniff(b"GIF89a.."), Some(("image/gif", "gif")));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(("image/webp", "webp")));
        assert_eq!(sniff(b"<svg xmlns="), None);
        assert_eq!(sniff(b"BM\0\0"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn images_become_markers_and_files_in_the_order_of_the_text() {
        let gif = b"GIF89a\x01\0\x01\0".to_vec();
        let req = request(vec![
            msg("system", "rules"),
            user_with_images(
                "что на снимках?",
                &[data_url("image/png", PNG), data_url("image/gif", &gif)],
            ),
        ]);
        let p = prepare(&req, 0, true);
        let (shown, files) = p.take_files();
        assert_eq!(
            shown.messages[1].text(),
            "что на снимках?\n[изображение 1 во вложении]\n[изображение 2 во вложении]"
        );
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "image-1.png");
        assert_eq!(files[0].mime, "image/png");
        assert_eq!(decode_base64(&files[0].data_base64).unwrap(), PNG);
        assert_eq!(files[1].name, "image-2.gif");
        assert_eq!(files[1].mime, "image/gif");
        // The original text reads as before: images were never in it.
        assert_eq!(req.messages[1].text(), "что на снимках?");
        // The system message and the count are kept.
        assert_eq!(shown.messages.len(), 2);
        assert_eq!(shown.messages[0].text(), "rules");
    }

    #[test]
    fn the_declared_type_does_not_count_the_bytes_do() {
        let req = request(vec![user_with_images(
            "x",
            &[
                // A PNG declared as JPEG goes as PNG.
                data_url("image/jpeg", PNG),
                // An SVG (or anything else) is not sent.
                data_url("image/svg+xml", b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
            ],
        )]);
        let (shown, files) = prepare(&req, 0, true).take_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].mime, "image/png");
        let text = shown.messages[0].text();
        assert!(text.contains("[изображение 1 во вложении]"), "{text}");
        assert!(
            text.contains("тип image/svg+xml не поддерживается"),
            "{text}"
        );
    }

    #[test]
    fn remote_urls_are_not_fetched_and_named_in_a_note() {
        let req = request(vec![user_with_images(
            "x",
            &["https://example.com/a.png?x=1".to_string()],
        )]);
        let (shown, files) = prepare(&req, 0, true).take_files();
        assert!(files.is_empty());
        assert_eq!(
            shown.messages[0].text(),
            "x\n[изображение по ссылке не отправлено: шлюз не загружает ссылки: \
             https://example.com/a.png?x=1]"
        );
    }

    #[test]
    fn file_parts_with_an_image_go_and_other_files_become_a_note() {
        let mut m = user_with_images("x", &[]);
        if let Some(Content::Parts(parts)) = &mut m.content {
            parts.push(Part {
                kind: "file".into(),
                file: Some(json!({"filename": "shot.png", "file_data": data_url("image/png", PNG)})),
                ..Default::default()
            });
            parts.push(Part {
                kind: "file".into(),
                file: Some(json!({"filename": "spec.pdf", "file_data": "data:application/pdf;base64,JVBERi0xLjQK"})),
                ..Default::default()
            });
            parts.push(Part {
                kind: "video_url".into(),
                ..Default::default()
            });
        }
        let (shown, files) = prepare(&request(vec![m]), 0, true).take_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "image-1.png");
        let text = shown.messages[0].text();
        assert!(text.contains("[изображение 1 во вложении]"), "{text}");
        assert!(
            text.contains("[файл spec.pdf не отправлен: тип application/pdf не поддерживается"),
            "{text}"
        );
        assert!(
            text.contains("[вложение типа video_url не отправлено]"),
            "{text}"
        );
    }

    #[test]
    fn an_old_extension_gets_notes_and_no_files_whatever_the_size() {
        let big = data_url("image/png", &png_of(MAX_ASK_IMAGE_BYTES * 2));
        let req = request(vec![user_with_images("x", &[big])]);
        let (shown, files) = prepare(&req, 0, false).take_files();
        assert!(files.is_empty());
        assert_eq!(
            shown.messages[0].text(),
            format!("x\n{}", OLD_EXTENSION_NOTE)
        );
    }

    #[test]
    fn images_over_a_limit_become_notes_and_the_rest_go() {
        // One image over the limit by itself: a note with its size.
        let big = request(vec![user_with_images(
            "x",
            &[data_url("image/png", &png_of(2_200_000))],
        )]);
        let (shown, files) = prepare(&big, 0, true).take_files();
        assert!(files.is_empty());
        assert_eq!(
            shown.messages[0].text(),
            "x\n[изображение не отправлено: 2,1 МБ, предел 700 КБ на сообщение]"
        );
        // Two that fit alone but not together: the newer goes.
        let half = MAX_ASK_IMAGE_BYTES / 2 + 1;
        let two = request(vec![user_with_images(
            "x",
            &[
                data_url("image/png", &png_of(half)),
                data_url("image/png", &png_of(half)),
            ],
        )]);
        let (shown, files) = prepare(&two, 0, true).take_files();
        assert_eq!(files.len(), 1);
        assert_eq!(
            shown.messages[0].text(),
            "x\n[изображение не отправлено: 351 КБ, вместе с более новыми больше предела 700 КБ \
             на сообщение]\n[изображение 1 во вложении]"
        );
        // Eleven: the oldest one is a note.
        let many = request(vec![user_with_images(
            "x",
            &(0..=MAX_IMAGES)
                .map(|_| data_url("image/png", PNG))
                .collect::<Vec<_>>(),
        )]);
        let (shown, files) = prepare(&many, 0, true).take_files();
        assert_eq!(files.len(), MAX_IMAGES);
        assert!(shown.messages[0]
            .text()
            .starts_with("x\n[изображение не отправлено: больше 10 изображений в одном сообщении]\n[изображение 1 во вложении]"));
    }

    #[test]
    fn earlier_images_of_a_bootstrap_go_newest_first_while_they_fit() {
        let third = MAX_ASK_IMAGE_BYTES / 3;
        let req = request(vec![
            user_with_images("old 1", &[data_url("image/png", &png_of(third))]),
            msg("assistant", "a1"),
            user_with_images("old 2", &[data_url("image/png", &png_of(third))]),
            msg("assistant", "a2"),
            user_with_images(
                "new",
                &[
                    data_url("image/png", &png_of(third)),
                    data_url("image/png", PNG),
                ],
            ),
        ]);
        let (shown, files) = prepare(&req, 0, true).take_files();
        // The new two and "old 2" fit; "old 1" does not.
        assert_eq!(files.len(), 3);
        assert!(shown.messages[0]
            .text()
            .contains("вместе с более новыми больше предела 700 КБ"));
        assert_eq!(
            shown.messages[2].text(),
            "old 2\n[изображение 1 во вложении]"
        );
        assert_eq!(
            shown.messages[4].text(),
            "new\n[изображение 2 во вложении]\n[изображение 3 во вложении]"
        );
    }

    #[test]
    fn images_before_the_sent_messages_are_dropped_from_the_shown_request() {
        let req = request(vec![
            user_with_images("seen", &[data_url("image/png", PNG)]),
            msg("assistant", "a1"),
            user_with_images("delta", &[data_url("image/png", PNG)]),
        ]);
        let (shown, files) = prepare(&req, 2, true).take_files();
        assert_eq!(files.len(), 1);
        assert_eq!(shown.messages[0].text(), "seen");
        match &shown.messages[0].content {
            Some(Content::Parts(parts)) => assert!(parts.iter().all(|p| p.image_url.is_none())),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            shown.messages[2].text(),
            "delta\n[изображение 1 во вложении]"
        );
    }

    #[test]
    fn the_text_room_leaves_the_files_and_the_reserve_out_of_the_frame() {
        assert_eq!(text_room(&[]), usize::MAX);
        let file = Attachment {
            name: "image-1.png".into(),
            mime: "image/png".into(),
            data_base64: encode_base64(&png_of(MAX_ASK_IMAGE_BYTES)),
        };
        let room = text_room(std::slice::from_ref(&file));
        assert!(room >= MIN_TEXT_ROOM, "{room}");
        let files_json = serde_json::to_vec(&[file]).unwrap().len();
        assert!(room + files_json + FRAME_RESERVE <= MAX_FRAME_BYTES as usize);
    }
}
