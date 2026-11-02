//! Shared corpus of fenced-block inputs with their expected top-level split,
//! for the tests of every reader of ChatGPT's text: the scanner itself, the
//! agent protocol parser, the CLI renderer and the gateway reply parser.
//! Compiled only for tests and with the `test-support` feature (enabled by
//! the other crates as a dev-dependency), never into a release build.
//!
//! Agent bodies are a valid `final` response; `tool_calls` bodies are a valid
//! call of the gateway test tool `read` with `{"path": "a"}`.

/// One input and the top-level blocks the scanner must find in it.
pub struct Case {
    pub name: &'static str,
    pub input: &'static str,
    pub blocks: &'static [Expected],
}

/// An expected top-level block.
pub struct Expected {
    /// The fence character, '`' or '~'.
    pub fence: char,
    /// The whitespace before the opening fence on its line.
    pub indent: &'static str,
    /// The trimmed info string.
    pub info: &'static str,
    /// The lines between the fences, each with its line ending.
    pub body: &'static str,
    pub closed: bool,
}

macro_rules! agent_json {
    () => {
        r#"{"version":1,"status":"final","user_message":"done","plan_update":{"summary":"","events":[]},"command":null}"#
    };
}

macro_rules! calls_json {
    () => {
        r#"[{"name":"read","arguments":{"path":"a"}}]"#
    };
}

pub const AGENT_INFO: &str = "cgpt-agent-response-v1";

pub const FENCE_CORPUS: &[Case] = &[
    Case {
        name: "prose only",
        input: "just prose\nno fences\n",
        blocks: &[],
    },
    Case {
        name: "agent block between prose",
        input: concat!(
            "hello\n```cgpt-agent-response-v1\n",
            agent_json!(),
            "\n```\nbye\n"
        ),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: AGENT_INFO,
            body: concat!(agent_json!(), "\n"),
            closed: true,
        }],
    },
    Case {
        name: "nested example inside a four-backtick markdown block",
        input: concat!(
            "Format:\n````markdown\n```cgpt-agent-response-v1\n",
            agent_json!(),
            "\n```\n````\nDone.\n"
        ),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: "markdown",
            body: concat!("```cgpt-agent-response-v1\n", agent_json!(), "\n```\n"),
            closed: true,
        }],
    },
    Case {
        name: "tool_calls inside markdown",
        input: concat!(
            "Example:\n```markdown\nCall like this:\n~~~tool_calls\n",
            calls_json!(),
            "\n~~~\n```\n"
        ),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: "markdown",
            body: concat!("Call like this:\n~~~tool_calls\n", calls_json!(), "\n~~~\n"),
            closed: true,
        }],
    },
    Case {
        name: "tilde block holding backtick fences",
        input: concat!("~~~text\n```tool_calls\n", calls_json!(), "\n```\n~~~\n"),
        blocks: &[Expected {
            fence: '~',
            indent: "",
            info: "text",
            body: concat!("```tool_calls\n", calls_json!(), "\n```\n"),
            closed: true,
        }],
    },
    Case {
        name: "four backticks closed by a longer run",
        input: concat!("````tool_calls\n", calls_json!(), "\n`````\n"),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: "tool_calls",
            body: concat!(calls_json!(), "\n"),
            closed: true,
        }],
    },
    Case {
        name: "four backticks are not closed by three",
        input: concat!("````tool_calls\n", calls_json!(), "\n```\n"),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: "tool_calls",
            body: concat!(calls_json!(), "\n```\n"),
            closed: false,
        }],
    },
    Case {
        name: "unclosed block runs to the end",
        input: concat!("intro\n```cgpt-agent-response-v1\n", agent_json!(), "\n"),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: AGENT_INFO,
            body: concat!(agent_json!(), "\n"),
            closed: false,
        }],
    },
    Case {
        name: "two top-level blocks",
        input: concat!(
            "```json\n{}\n```\n```cgpt-agent-response-v1\n",
            agent_json!(),
            "\n```\n"
        ),
        blocks: &[
            Expected {
                fence: '`',
                indent: "",
                info: "json",
                body: "{}\n",
                closed: true,
            },
            Expected {
                fence: '`',
                indent: "",
                info: AGENT_INFO,
                body: concat!(agent_json!(), "\n"),
                closed: true,
            },
        ],
    },
    Case {
        name: "lookalike agent labels",
        input: concat!(
            "```cgpt-agent-response-v1x\n",
            agent_json!(),
            "\n```\n```CGPT-Agent-Response-V1\n",
            agent_json!(),
            "\n```\n"
        ),
        blocks: &[
            Expected {
                fence: '`',
                indent: "",
                info: "cgpt-agent-response-v1x",
                body: concat!(agent_json!(), "\n"),
                closed: true,
            },
            Expected {
                fence: '`',
                indent: "",
                info: "CGPT-Agent-Response-V1",
                body: concat!(agent_json!(), "\n"),
                closed: true,
            },
        ],
    },
    Case {
        name: "agent label on a tilde fence",
        input: concat!("~~~cgpt-agent-response-v1\n", agent_json!(), "\n~~~\n"),
        blocks: &[Expected {
            fence: '~',
            indent: "",
            info: AGENT_INFO,
            body: concat!(agent_json!(), "\n"),
            closed: true,
        }],
    },
    Case {
        name: "a backtick in the info string is not a fence",
        input: "```js`x\nnot code\n```\n",
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: "",
            body: "",
            closed: false,
        }],
    },
    Case {
        name: "CRLF agent block",
        input: concat!(
            "Run:\r\n```cgpt-agent-response-v1\r\n",
            agent_json!(),
            "\r\n```\r\nok\r\n"
        ),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: AGENT_INFO,
            body: concat!(agent_json!(), "\r\n"),
            closed: true,
        }],
    },
    Case {
        name: "CRLF tool_calls block",
        input: concat!("Read:\r\n```tool_calls\r\n", calls_json!(), "\r\n```\r\n"),
        blocks: &[Expected {
            fence: '`',
            indent: "",
            info: "tool_calls",
            body: concat!(calls_json!(), "\r\n"),
            closed: true,
        }],
    },
    Case {
        name: "agent fence in a list item, three spaces",
        input: concat!(
            "1. Step:\n\n   ```cgpt-agent-response-v1\n   ",
            agent_json!(),
            "\n   ```\n"
        ),
        blocks: &[Expected {
            fence: '`',
            indent: "   ",
            info: AGENT_INFO,
            body: concat!("   ", agent_json!(), "\n"),
            closed: true,
        }],
    },
    Case {
        name: "tool_calls fence indented two spaces",
        input: concat!("  ```tool_calls\n  ", calls_json!(), "\n  ```\n"),
        blocks: &[Expected {
            fence: '`',
            indent: "  ",
            info: "tool_calls",
            body: concat!("  ", calls_json!(), "\n"),
            closed: true,
        }],
    },
    Case {
        name: "agent fence indented four spaces is a code sample",
        input: concat!(
            "Example:\n\n    ```cgpt-agent-response-v1\n    ",
            agent_json!(),
            "\n    ```\n"
        ),
        blocks: &[Expected {
            fence: '`',
            indent: "    ",
            info: AGENT_INFO,
            body: concat!("    ", agent_json!(), "\n"),
            closed: true,
        }],
    },
    Case {
        name: "agent fence indented by a tab is a code sample",
        input: concat!(
            "\t```cgpt-agent-response-v1\n\t",
            agent_json!(),
            "\n\t```\n"
        ),
        blocks: &[Expected {
            fence: '`',
            indent: "\t",
            info: AGENT_INFO,
            body: concat!("\t", agent_json!(), "\n"),
            closed: true,
        }],
    },
    Case {
        name: "tool_calls fence indented four spaces",
        input: concat!("    ```tool_calls\n    ", calls_json!(), "\n    ```\n"),
        blocks: &[Expected {
            fence: '`',
            indent: "    ",
            info: "tool_calls",
            body: concat!("    ", calls_json!(), "\n"),
            closed: true,
        }],
    },
];
