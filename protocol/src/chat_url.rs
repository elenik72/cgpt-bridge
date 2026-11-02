//! What names a ChatGPT chat in its address, shared by the gateway (is the
//! answer from the session's chat?) and the CLI (is a `cgpt agent` reply from
//! the session's chat?). The extension compares the same way
//! (`targetMatchesPath`): a project's slug changes with its name.

/// What names a chat in a URL: its `<id>` of `/c/<id>`, wherever the path
/// puts it (`/c/<id>`, or `/g/<project>/c/<id>` in a ChatGPT project), without
/// the host, query, fragment and a trailing slash. A URL without `/c/<id>`
/// stands for itself (without query, fragment and a trailing slash).
pub fn chat_key(url: &str) -> String {
    chat_id(url).unwrap_or_else(|| bare_url(url)).to_string()
}

/// The `<id>` of the last `/c/<id>` in the URL's path.
pub fn chat_id(url: &str) -> Option<&str> {
    let path = bare_url(url)
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("");
    let segments: Vec<&str> = path.split('/').collect();
    segments
        .windows(2)
        .rev()
        .find(|w| w[0] == "c" && !w[1].is_empty())
        .map(|w| w[1])
}

fn bare_url(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
}
