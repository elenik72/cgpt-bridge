// What the adapter recovers from the page's DOM, on HTML snapshots of each
// layout ChatGPT serves (test/fixtures/dom/): readAssistantText,
// walkNodeForText and findCodeBlockLanguage, reached through the exported
// getLastAssistantMessage / getLastAssistantMessageId / detectProfile, on the
// fake window (test/harness/fakeWindow.ts) with snapshots parsed by
// test/harness/domSnapshot.ts.
//
// The snapshots are synthetic and minimal: they are built from the selectors
// and DOM shapes chatgptAdapter.ts documents (its header, "verified
// 2026-10-23"), not recorded from a live page, and hold no personal data. A
// change in ChatGPT's markup shows up here only after the matching snapshot
// is updated from a fresh look at the page (the adapter's repair playbook) —
// these tests pin what the adapter does with the shapes it knows.
//
// Tests marked `CHARACTERIZATION (...)` pin the CURRENT behaviour, including
// quirks nobody has decided to keep. They are a snapshot, not a contract:
// update them together with a deliberate change instead of treating the red
// test as a regression.

import { test, before } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  captureAnswerBaseline,
  detectProfile,
  findComposer,
  getLastAssistantMessage,
  getLastAssistantMessageId,
  isLoggedIn,
  lastTurnEnded,
  userTurnCount,
} from "../src/chatgptAdapter";
import { loadPage, resetPage } from "./harness/fakeWindow";
import type { FakeElement } from "./harness/fakeWindow";
import { loadSnapshot, parseHtml } from "./harness/domSnapshot";

// The test file and its bundle (.test-build/) both sit one level below
// extension/, so the same relative path works for either.
const FIXTURES = new URL("../test/fixtures/dom/", import.meta.url);

function show(name: string): void {
  resetPage("/c/snapshot");
  loadSnapshot(readFileSync(new URL(`${name}.html`, FIXTURES), "utf8"));
}

before(async () => {
  await loadPage();
});

// ─── "account" ────────────────────────────────────────────────────────────

test("domRecovery.account_reads_the_newest_answer_as_markdown", () => {
  show("account-answer");
  assert.equal(detectProfile(), "account");
  assert.equal(isLoggedIn(), true);
  assert.equal(getLastAssistantMessageId(), "msg-acc-2");
  assert.equal(
    getLastAssistantMessage(),
    [
      // The screen-reader h4 and the citation badge ("Rust Blog") are not text.
      "Rust is fast. See the list:",
      "one",
      "two bold",
      "",
      "",
      "```bash",
      "echo hello",
      "ls -la",
      "```",
      "Plain text:",
      "",
      // A header label that is not one token ("Plain text") gives no info string.
      "```",
      "no language here",
      "```",
      "",
      // A body that holds ``` lines gets a longer fence, so it stays one block.
      "````markdown",
      "```rust",
      "fn main() {}",
      "```",
      "````",
      "Done.",
    ].join("\n"),
  );
});

test("domRecovery.account_labels_tool_calls_and_protocol_blocks", () => {
  show("account-tool-calls");
  assert.equal(getLastAssistantMessageId(), "msg-tc-1");
  assert.equal(
    getLastAssistantMessage(),
    [
      "Calling tools with tool_calls:",
      "",
      "```tool_calls",
      '[{"name": "read", "arguments": {"path": "src/main.rs"}}]',
      "```",
      "",
      // A block that only mentions tool_calls in its body keeps its own label.
      "```json",
      '{"note": "the tool_calls block is next"}',
      "```",
      "",
      // A display-cased protocol label is normalized to the lower-case fence label.
      "```cgpt-agent-response-v1",
      '{"status": "done"}',
      "```",
      "",
      // The earlier variant of the markup: `pre > code` without data-code-block-source.
      "```tool_calls",
      '[{"name": "list"}]',
      "```",
    ].join("\n"),
  );
});

// ─── "account", as found live on 2026-10-28 ───────────────────────────────

test("domRecovery.account_2026_10_28_page_is_ready", () => {
  show("account-2026-10-28");
  assert.equal(detectProfile(), "account");
  assert.equal(isLoggedIn(), true);
  // The textbox itself, found without the ProseMirror class or the localized label.
  assert.equal(findComposer()?.getAttribute("data-composer-markdown"), "");
});

test("domRecovery.account_2026_10_28_counts_user_messages_by_their_bubble", () => {
  show("account-2026-10-28");
  // Four turn states (one per message here), two user messages.
  assert.equal(userTurnCount(), 2);
});

test("domRecovery.account_2026_10_28_reads_the_newest_answer_from_its_markdown_copy", () => {
  show("account-2026-10-28");
  // Nested id containers are one answer each.
  assert.deepEqual(captureAnswerBaseline(), { count: 2, messageId: "msg-new-2" });
  // The screen-reader h4 and the Copy bar outside [data-markdown-copy] are not text.
  assert.equal(
    getLastAssistantMessage(),
    ["First paragraph of the answer.", "one", "two bold", "", "Last paragraph."].join("\n"),
  );
});

test("domRecovery.account_2026_10_28_turn_end_is_complete_or_cancelled_only", () => {
  show("account-2026-10-28");
  assert.equal(lastTurnEnded(), true);
  const turns = document.querySelectorAll<HTMLElement>("[data-talvt-turn-state]");
  const last = turns[turns.length - 1]!;
  // The values a turn shows while it runs were not recorded: any other one is "not ended".
  for (const state of ["in_progress", "streaming", ""]) {
    last.setAttribute("data-talvt-turn-state", state);
    assert.equal(lastTurnEnded(), false, state);
  }
  last.setAttribute("data-talvt-turn-state", "cancelled");
  assert.equal(lastTurnEnded(), true);
  // A markup without turn states says nothing.
  show("classic-answer");
  assert.equal(lastTurnEnded(), null);
});

test("domRecovery.account_2026_10_28_id_container_around_a_user_message_is_no_answer", () => {
  resetPage("/c/snapshot");
  loadSnapshot(`
    <main>
      <div data-chatgpt-selection-message-id="msg-user-1">
        <div data-user-message-bubble=""><div>A question</div></div>
      </div>
      <div data-chatgpt-selection-message-id="msg-turn-2">
        <div data-user-message-bubble=""><div>Another question</div></div>
        <div data-dil-source-message-id="msg-answer-2">
          <div data-markdown-copy=""><p data-d-text-flow="">The answer.</p></div>
        </div>
      </div>
    </main>`);
  assert.deepEqual(captureAnswerBaseline(), { count: 1, messageId: "msg-answer-2" });
  assert.equal(getLastAssistantMessage(), "The answer.");
});

// The stream sends a reasoning recap as a message of its own before the
// answer (2026-10-28 sample). Its block may carry a message id; without the
// answer's markdown it is no answer, so the recap alone is not a reply.
test("domRecovery.account_2026_10_28_reasoning_recap_is_no_answer", () => {
  resetPage("/c/snapshot");
  loadSnapshot(`
    <main>
      <div data-thread="">
        <div data-user-message-bubble=""><div>A question</div></div>
        <div data-dil-source-message-id="msg-recap"><div>Thought for a second</div></div>
      </div>
    </main>`);
  assert.deepEqual(captureAnswerBaseline(), { count: 0, messageId: null });
  assert.equal(getLastAssistantMessage(), null);
  (document.querySelector("[data-thread]") as unknown as FakeElement).append(
    ...parseHtml(`<div data-dil-source-message-id="msg-answer">
      <div data-markdown-copy=""><p data-d-text-flow="">The answer.</p></div></div>`),
  );
  assert.deepEqual(captureAnswerBaseline(), { count: 1, messageId: "msg-answer" });
  assert.equal(getLastAssistantMessage(), "The answer.");
});

// Two markdown roots in one answer (a recap or search step above the final
// channel): the answer is the last one, not the first.
test("domRecovery.account_2026_10_28_two_markdown_roots_read_the_last", () => {
  resetPage("/c/snapshot");
  loadSnapshot(`
    <main>
      <div data-user-message-bubble=""><div>A question</div></div>
      <div data-chatgpt-selection-message-id="msg-a">
        <div data-markdown-copy=""><p data-d-text-flow="">Searched three sites.</p></div>
        <div data-markdown-copy=""><p data-d-text-flow="">The answer.</p><p data-d-text-flow="">In full.</p></div>
      </div>
    </main>`);
  assert.deepEqual(captureAnswerBaseline(), { count: 1, messageId: "msg-a" });
  assert.equal(getLastAssistantMessage(), "The answer.\nIn full.");
});

// The newest user message has no answer yet: the answer above it belongs to
// the exchange before and is not the newest answer, on either markup.
test("domRecovery.answer_above_the_newest_user_message_is_not_the_newest", () => {
  resetPage("/c/snapshot");
  loadSnapshot(`
    <main>
      <div data-user-message-bubble=""><div>First question</div></div>
      <div data-dil-source-message-id="msg-1"><div data-markdown-copy=""><p>First answer.</p></div></div>
      <div data-user-message-bubble=""><div>Second question</div></div>
    </main>`);
  assert.equal(getLastAssistantMessageId(), null);
  assert.equal(getLastAssistantMessage(), null);
  resetPage("/c/snapshot");
  loadSnapshot(`
    <main>
      <div data-message-author-role="user">First question</div>
      <div data-message-author-role="assistant" data-message-id="msg-1"><div class="markdown"><p>First answer.</p></div></div>
      <div data-message-author-role="user">Second question</div>
    </main>`);
  assert.equal(getLastAssistantMessageId(), null);
  assert.equal(getLastAssistantMessage(), null);
});

// ─── "octane" ─────────────────────────────────────────────────────────────

test("domRecovery.octane_reads_the_markdown_root_without_reasoning", () => {
  show("octane-answer");
  assert.equal(detectProfile(), "octane");
  // A gateway turn is refused on the signed-out markup (TASK-014).
  assert.equal(isLoggedIn(), false);
  assert.equal(
    getLastAssistantMessage(),
    ["Hello from octane.", "first", "second", "", "", "```python", 'print("hi")', "```"].join("\n"),
  );
  // CHARACTERIZATION (no planned task): the octane markup carries no message id attribute, so the
  // id is null and a new answer is told apart by the answer count (captureAnswerBaseline).
  assert.equal(getLastAssistantMessageId(), null);
});

test("domRecovery.octane_reasoning_alone_is_no_answer", () => {
  show("octane-reasoning-only");
  assert.equal(detectProfile(), "octane");
  assert.equal(getLastAssistantMessage(), null);
});

// ─── "classic" ────────────────────────────────────────────────────────────

test("domRecovery.classic_reads_the_markdown_container", () => {
  show("classic-answer");
  assert.equal(detectProfile(), "classic");
  assert.equal(isLoggedIn(), true);
  assert.equal(getLastAssistantMessageId(), "msg-cl-1");
  assert.equal(
    getLastAssistantMessage(),
    [
      // The sr-only "ChatGPT said:" label is outside .markdown.
      "Line one",
      "line two",
      "Heading",
      "quoted",
      "",
      "",
      // The language-* class names the fence (other classes on the element do not matter).
      "```python",
      "def f():",
      "    return 1",
      "```",
      "",
      // A bare pre takes tool_calls from its own header (the <div> right before it). The header's
      // text (label and "Copy" button) is not in the answer.
      "```tool_calls",
      '[{"name": "read"}]',
      "```",
      "",
      // Any other one-token header ("Python") is the fence's language, lower-cased; the header's
      // text is left out like any other header's.
      "```python",
      'print("unlabelled")',
      "```",
      "Closing text.",
    ].join("\n"),
  );
});

// ─── CodeMirror (classic) ─────────────────────────────────────────────────

test("domRecovery.codemirror_rebuilds_fences_from_spans_and_line_breaks", () => {
  show("classic-codemirror");
  assert.equal(detectProfile(), "classic");
  assert.equal(getLastAssistantMessageId(), "msg-cm-1");
  assert.equal(
    getLastAssistantMessage(),
    [
      "Agent reply:",
      // In the CodeMirror shape the header sits next to the viewer's wrapper. Its text (label and
      // "Copy code" button) is not in the answer; the label goes on the fence.
      "",
      // <span> tokens joined, each <br> is a newline; the label is the protocol one.
      "```cgpt-agent-response-v1",
      "{",
      '  "status": "done"',
      "}",
      "```",
    ].join("\n"),
  );
});

test("domRecovery.codemirror_tool_calls_header_labels_the_fence", () => {
  show("classic-codemirror-tool-calls");
  assert.equal(getLastAssistantMessageId(), "msg-cm-2");
  assert.equal(
    getLastAssistantMessage(),
    [
      // The header labels the fence; its text ("tool_calls", "Copy code") is not in the answer.
      "```tool_calls",
      '[{"name": "read"}]',
      "```",
    ].join("\n"),
  );
});

test("domRecovery.codemirror_other_language_labels_the_fence", () => {
  show("classic-codemirror-other-language");
  assert.equal(
    getLastAssistantMessage(),
    [
      // An ordinary language header ("rust") is the fence's language; its text is not in the answer.
      "```rust",
      "fn main() {}",
      "```",
    ].join("\n"),
  );
});

test("domRecovery.codemirror_each_block_takes_its_own_label", () => {
  show("classic-codemirror-two-blocks");
  // Each fence carries its own block's label: the answer container's text, which holds the first
  // block's protocol label, does not label the second block.
  assert.equal(
    getLastAssistantMessage(),
    [
      "```cgpt-agent-response-v1",
      '{"status": "done"}',
      "```",
      "",
      "",
      "",
      "```tool_calls",
      "[]",
      "```",
    ].join("\n"),
  );
});

test("domRecovery.codemirror_label_comes_from_no_code_body", () => {
  resetPage("/c/snapshot");
  // A "json" block whose body names a protocol label, then a block with no header at all.
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-message-author-role="assistant" data-message-id="msg-cm-5">
      <div class="markdown prose">
        <div class="block">
          <div class="header"><span>json</span></div>
          <div><div class="cm-editor"><pre class="cm-content"><code><span>{"format": "cgpt-agent-response-v1"}</span></code></pre></div></div>
        </div>
        <div class="block">
          <div><div class="cm-editor"><pre class="cm-content"><code><span>[]</span></code></pre></div></div>
        </div>
      </div>
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    [
      // A protocol label inside a block's own code labels neither that block nor the next one; the
      // first block keeps its own header's language.
      "```json",
      '{"format": "cgpt-agent-response-v1"}',
      "```",
      "",
      "",
      "",
      "```",
      "[]",
      "```",
    ].join("\n"),
  );
});

test("domRecovery.inline_tool_calls_before_a_header_less_block_is_no_label", () => {
  resetPage("/c/snapshot");
  // Paragraphs that end in inline `tool_calls`, each followed by a block with no header (a bare
  // pre, then a CodeMirror viewer), then a block with its own `tool_calls` header.
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-message-author-role="assistant" data-message-id="msg-cm-6">
      <div class="markdown prose">
        <p>The answer goes in <code>tool_calls</code></p>
        <pre><code>[{"name": "x"}]</code></pre>
        <div><code>tool_calls</code></div>
        <pre><code>[1]</code></pre>
        <p>Or a viewer after <code>tool_calls</code></p>
        <div><div class="cm-editor"><pre class="cm-content"><code><span>[]</span></code></pre></div></div>
        <p>A real one after <code>tool_calls</code></p>
        <div class="block">
          <div class="header"><span>tool_calls</span><button><span>Copy code</span></button></div>
          <div><div class="cm-editor"><pre class="cm-content"><code><span>[{"name": "read"}]</span></code></pre></div></div>
        </div>
      </div>
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    [
      "The answer goes in tool_calls",
      "",
      // Prose before a block is not its header: the block has no label.
      "```",
      '[{"name": "x"}]',
      "```",
      // Inline code is never a header, even outside a paragraph: it stays text, the block is unlabelled.
      "tool_calls",
      "",
      "```",
      "[1]",
      "```",
      "Or a viewer after tool_calls",
      "",
      "```",
      "[]",
      "```",
      "",
      "A real one after tool_calls",
      "",
      // The block's own header labels it, and its text (label and button) is not in the answer.
      "```tool_calls",
      '[{"name": "read"}]',
      "```",
    ].join("\n"),
  );
});

test("domRecovery.protocol_label_in_prose_does_not_label_a_header_less_block", () => {
  resetPage("/c/snapshot");
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-message-author-role="assistant" data-message-id="msg-cm-7">
      <div class="markdown prose">
        <p>Reply with a cgpt-agent-response-v1 block, for example:</p>
        <pre><code>{"status": "done"}</code></pre>
      </div>
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    [
      "Reply with a cgpt-agent-response-v1 block, for example:",
      "",
      "```",
      '{"status": "done"}',
      "```",
    ].join("\n"),
  );
});

test("domRecovery.header_over_a_group_of_blocks_labels_none", () => {
  resetPage("/c/snapshot");
  // A header-like <div> before a wrapper that holds two blocks is neither block's own header.
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-message-author-role="assistant" data-message-id="msg-cm-8">
      <div class="markdown prose">
        <div class="header"><span>tool_calls</span></div>
        <div>
          <pre><code>[{"name": "a"}]</code></pre>
          <pre><code>[{"name": "b"}]</code></pre>
        </div>
      </div>
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    ["tool_calls", "", "```", '[{"name": "a"}]', "```", "", "```", '[{"name": "b"}]', "```"].join("\n"),
  );
});

test("domRecovery.content_div_before_a_block_is_not_a_header", () => {
  resetPage("/c/snapshot");
  // Divs right before a block that are answer content, not a header: no Copy button, and either
  // more than one plain token or rich content (bold, link, math, image, rule, a nested div).
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-message-author-role="assistant" data-message-id="msg-cm-9">
      <div class="markdown prose">
        <div>Here is the code:</div>
        <pre><code>a</code></pre>
        <div><strong>Note</strong></div>
        <pre><code>b</code></pre>
        <div><a href="https://example.com/">docs</a></div>
        <pre><code>c</code></pre>
        <div class="math math-display"><span class="katex-display"><span class="katex">x2</span></span></div>
        <pre><code>d</code></pre>
        <div><div>Result</div></div>
        <pre><code>e</code></pre>
        <div><img src="chart.png" alt="">Diagram</div>
        <pre><code>f</code></pre>
        <div>Summary<hr></div>
        <pre><code>g</code></pre>
      </div>
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    [
      "Here is the code:",
      "",
      "```",
      "a",
      "```",
      "Note",
      "",
      "```",
      "b",
      "```",
      "docs",
      "",
      "```",
      "c",
      "```",
      "x2",
      "",
      "```",
      "d",
      "```",
      "Result",
      "",
      "",
      "```",
      "e",
      "```",
      "Diagram",
      "",
      "```",
      "f",
      "```",
      "Summary",
      "",
      "```",
      "g",
      "```",
    ].join("\n"),
  );
});

test("domRecovery.prose_div_naming_a_protocol_label_is_not_a_header", () => {
  resetPage("/c/snapshot");
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-message-author-role="assistant" data-message-id="msg-cm-10">
      <div class="markdown prose">
        <div>Use cgpt-agent-response-v1 like so</div>
        <pre><code>{"status": "done"}</code></pre>
      </div>
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    ["Use cgpt-agent-response-v1 like so", "", "```", '{"status": "done"}', "```"].join("\n"),
  );
});

test("domRecovery.account_header_label_is_the_info_string_only_as_one_token", () => {
  resetPage("/c/snapshot");
  // Account markup: a plain label, a label with a space, with a backtick, one that starts with the
  // protocol label and goes on, the exact protocol label display-cased, and a 33-character one.
  const block = (label: string, code: string): string =>
    `<div data-code-block="true"><div data-markdown-copy="exclude"><span>${label}</span></div>` +
    `<pre data-code-block-source=""><code>${code}</code></pre></div>`;
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-dil-source-message-id="msg-acc-label" data-markdown-copy="">
      ${block("C++", "one")}
      ${block("Plain text", "two")}
      ${block("a`b", "three")}
      ${block("tool_calls json", "four")}
      ${block("Tool_Calls", "[]")}
      ${block("abcdefghijklmnopqrstuvwxyz0123456", "five")}
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    [
      "```c++",
      "one",
      "```",
      "",
      // Not one token: no info string, so no backtick breaks the opening line and no label that
      // merely starts with `tool_calls` reads as the gateway's.
      "```",
      "two",
      "```",
      "",
      "```",
      "three",
      "```",
      "",
      "```",
      "four",
      "```",
      "",
      // The exact protocol label stays recognised.
      "```tool_calls",
      "[]",
      "```",
      "",
      "```",
      "five",
      "```",
    ].join("\n"),
  );
});

test("domRecovery.header_language_is_the_fence_info_string_when_it_is_one_token", () => {
  resetPage("/c/snapshot");
  // Headers with a Copy button: a display-cased language, a label with a space, a label with a
  // backtick, a label over 32 characters, and the exact protocol label.
  loadSnapshot(`
<main>
  <div data-scroll-root="">
    <div data-message-author-role="assistant" data-message-id="msg-cm-11">
      <div class="markdown prose">
        <div class="header"><span>C++</span><button><span>Copy code</span></button></div>
        <pre><code>int main() {}</code></pre>
        <div class="header"><span>Plain text</span><button><span>Copy code</span></button></div>
        <pre><code>plain</code></pre>
        <div class="header"><span>a\`b</span><button><span>Copy code</span></button></div>
        <pre><code>tick</code></pre>
        <div class="header"><span>abcdefghijklmnopqrstuvwxyz0123456</span><button><span>Copy code</span></button></div>
        <pre><code>long</code></pre>
        <div class="header"><span>Tool_Calls</span><button><span>Copy code</span></button></div>
        <pre><code>[]</code></pre>
      </div>
    </div>
  </div>
  <div id="prompt-textarea" contenteditable="true"></div>
</main>`);
  assert.equal(
    getLastAssistantMessage(),
    [
      // A one-token label (A-Z a-z 0-9 + # . _ -, at most 32) is the info string, lower-cased.
      "```c++",
      "int main() {}",
      "```",
      "",
      // Anything else gives no info string; the header is still chrome and stays out of the answer.
      "```",
      "plain",
      "```",
      "",
      "```",
      "tick",
      "```",
      "",
      "```",
      "long",
      "```",
      "",
      "```tool_calls",
      "[]",
      "```",
    ].join("\n"),
  );
});
