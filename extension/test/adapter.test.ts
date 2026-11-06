// Pure-logic tests for how the adapter rebuilds fenced code blocks.

import { test } from "node:test";
import assert from "node:assert/strict";

import { fenceFor, protocolLanguageOf } from "../src/chatgptAdapter";

test("adapter.protocol_language_recovers_tool_calls_label", () => {
  assert.equal(protocolLanguageOf("tool_calls"), "tool_calls");
  assert.equal(protocolLanguageOf(" Tool_Calls "), "tool_calls");
  assert.equal(protocolLanguageOf("TOOL_CALLS"), "tool_calls");
  assert.equal(protocolLanguageOf("cgpt-agent-response-v1"), "cgpt-agent-response-v1");
  assert.equal(protocolLanguageOf("CGPT-Command-Result-V1"), "cgpt-command-result-v1");
  // Mentions in prose are not a label.
  assert.equal(protocolLanguageOf("ответь блоком tool_calls"), null);
  assert.equal(protocolLanguageOf("tool_calls_v2"), null);
  assert.equal(protocolLanguageOf("json"), null);
  // Attributes ChatGPT adds to the info string (live 05.11).
  assert.equal(protocolLanguageOf('tool_calls id="me-review-prep"'), "tool_calls");
  assert.equal(protocolLanguageOf("tool_calls id=a1 lang='json'"), "tool_calls");
  assert.equal(protocolLanguageOf("tool_calls и ещё текст"), null);
});

test("adapter.fence_outgrows_backtick_runs_in_the_body", () => {
  assert.equal(fenceFor('[{"name": "read"}]\n'), "```");
  assert.equal(fenceFor("```rust\nfn main() {}\n```\n"), "````");
  assert.equal(fenceFor("  ````md\nx\n````\n"), "`````");
  // Backticks inside a line do not count.
  assert.equal(fenceFor('{"patchText": "+```rust\\n+```"}\n'), "```");
});
