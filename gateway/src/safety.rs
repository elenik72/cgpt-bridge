//! What the gateway never hands to OpenCode, whatever its permissions say: a
//! command the denylist blocks, and a write of the redaction marker over a
//! real secret. Also which tools ChatGPT is shown at all: `execute` runs code
//! the denylist cannot see, so it stays hidden unless `allow_tools` names it.

use cgpt_bridge_core::denylist::classify;
use serde_json::Value;

use crate::openai::Tool;

/// The start of what `core::redact` puts in place of a secret
/// (`«redacted:<rule>»`).
pub const REDACTION_MARKER: &str = "«redacted:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallCheck {
    Pass,
    /// Not handed to OpenCode; the reason is for ChatGPT.
    Deny(String),
}

/// Check one call ChatGPT made. Every string `command` in the arguments goes
/// through the denylist, whatever the tool is called (on Windows the shell
/// tool has another name, and a tool may nest calls). A string anywhere in
/// the arguments that holds the redaction marker would overwrite the real
/// secret with the marker.
pub fn check_call(name: &str, args: &Value) -> CallCheck {
    let mut commands = Vec::new();
    let mut marked = false;
    scan(args, &mut commands, &mut marked);
    let mut rules: Vec<&str> = Vec::new();
    for hit in commands.iter().flat_map(|c| classify(c).blocks) {
        if !rules.contains(&hit.rule_name) {
            rules.push(hit.rule_name);
        }
    }
    if !rules.is_empty() {
        return CallCheck::Deny(format!(
            "вызов {}: команда запрещена правилами: {}; предложи другой способ",
            name,
            rules.join(", ")
        ));
    }
    if marked {
        return CallCheck::Deny(format!(
            "вызов {}: в аргументах есть маркер вычищенного секрета {}…». Ты видишь \
             только маркер, а не настоящее значение, и записал бы маркер поверх секрета. \
             Не переписывай строки с маркером и не бери их в контекст патча: меняй только \
             другие строки",
            name, REDACTION_MARKER
        ));
    }
    CallCheck::Pass
}

/// Collect the commands in `value` (a `command` key in any case, a string
/// or an argv array joined with spaces) and whether a string holds the
/// redaction marker.
fn scan(value: &Value, commands: &mut Vec<String>, marked: &mut bool) {
    match value {
        Value::String(s) => *marked |= s.contains(REDACTION_MARKER),
        Value::Array(items) => items.iter().for_each(|v| scan(v, commands, marked)),
        Value::Object(map) => {
            for (key, v) in map {
                if key.eq_ignore_ascii_case("command") {
                    match v {
                        Value::String(c) => commands.push(c.clone()),
                        Value::Array(argv) => commands.push(
                            argv.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        _ => {}
                    }
                }
                scan(v, commands, marked);
            }
        }
        _ => {}
    }
}

/// The message to ChatGPT when calls of its answer were refused: none of
/// them reached OpenCode.
pub fn refusal(reasons: &[String]) -> String {
    format!(
        "Шлюз не передал вызовы инструментов из твоего ответа, ни один не выполнен:\n{}\n\n\
         Пришли вызовы заново без этого. Если иначе задачу не решить, ответь пользователю \
         текстом без блока tool_calls.",
        reasons
            .iter()
            .map(|r| format!("- {}", r))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The tools ChatGPT is shown and may call: all but `execute`, which only
/// when `allow` names it.
pub fn visible_tools(tools: &[Tool], allow: &[String]) -> Vec<Tool> {
    tools
        .iter()
        .filter(|t| t.function.name != "execute" || allow.iter().any(|a| a == "execute"))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::ToolFunction;
    use serde_json::json;

    fn tool(name: &str) -> Tool {
        Tool {
            kind: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: None,
                parameters: json!({"type": "object"}),
            },
        }
    }

    fn denied(check: CallCheck) -> String {
        match check {
            CallCheck::Deny(reason) => reason,
            CallCheck::Pass => panic!("passed"),
        }
    }

    #[test]
    fn allowed_command_passes() {
        for command in [
            "ls -la",
            "git status",
            "Get-ChildItem",
            "cargo test --workspace",
        ] {
            assert_eq!(
                check_call("bash", &json!({ "command": command })),
                CallCheck::Pass,
                "{}",
                command
            );
        }
        assert_eq!(
            check_call("read", &json!({ "filePath": "/etc/hosts" })),
            CallCheck::Pass
        );
    }

    #[test]
    fn command_param_checked_for_any_tool_name() {
        for name in ["bash", "shell", "powershell", "run_in_terminal"] {
            let reason = denied(check_call(
                name,
                &json!({ "command": r"Remove-Item -Recurse -Force C:\", "description": "x" }),
            ));
            assert!(reason.contains("команда запрещена правилами"), "{}", reason);
            assert!(
                reason.contains("recursive_delete_root_or_profile"),
                "{}",
                reason
            );
            assert!(reason.contains(name), "{}", reason);
        }
        // A command nested deeper (a tool that batches calls) is checked too.
        let nested =
            json!({ "calls": [{ "tool": "bash", "parameters": { "command": "rm -rf /" } }] });
        assert!(denied(check_call("batch", &nested)).contains("запрещена"));
    }

    #[test]
    fn command_as_argv_array_and_any_key_case_is_checked() {
        let argv = json!({ "command": ["rm", "-rf", "/"] });
        assert!(denied(check_call("exec", &argv)).contains("запрещена"));
        let upper = json!({ "Command": "rm -rf /" });
        assert!(denied(check_call("powershell", &upper)).contains("запрещена"));
        let nested =
            json!({ "steps": [{ "COMMAND": ["Remove-Item", "-Recurse", "-Force", r"C:\"] }] });
        assert!(denied(check_call("batch", &nested)).contains("запрещена"));
        assert_eq!(
            check_call("exec", &json!({ "command": ["ls", "-la"] })),
            CallCheck::Pass
        );
    }

    #[test]
    fn redaction_marker_in_any_string_of_the_args_is_denied() {
        let secret = cgpt_bridge_core::redact::redact("API_KEY=hunter2-very-secret", "").stdout;
        assert!(
            secret.contains(REDACTION_MARKER),
            "core's marker: {}",
            secret
        );
        let args = json!({ "patchText": format!("*** Update File: .env\n-OLD=1\n+{}\n", secret) });
        let reason = denied(check_call("patch", &args));
        assert!(reason.contains("маркер"), "{}", reason);
        assert!(reason.contains("patch"), "{}", reason);
        assert!(reason.contains("не бери их в контекст патча"), "{}", reason);
        let nested = json!({ "edits": [{ "newString": secret }] });
        assert!(denied(check_call("multiedit", &nested)).contains("маркер"));
    }

    #[test]
    fn refusal_lists_every_reason_and_asks_for_another_way() {
        let text = refusal(&["первая".to_string(), "вторая".to_string()]);
        assert!(
            text.contains("первая") && text.contains("вторая"),
            "{}",
            text
        );
        assert!(text.contains("не передал"), "{}", text);
    }

    #[test]
    fn execute_hidden_unless_allowed() {
        let tools = [tool("read"), tool("execute"), tool("bash")];
        let names =
            |v: Vec<Tool>| -> Vec<String> { v.into_iter().map(|t| t.function.name).collect() };
        assert_eq!(names(visible_tools(&tools, &[])), ["read", "bash"]);
        assert_eq!(
            names(visible_tools(&tools, &["execute".to_string()])),
            ["read", "execute", "bash"]
        );
    }
}
