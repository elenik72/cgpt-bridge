// Audit probe E4 (AUD-005, research/CODE_AUDIT.md) on the fake window: a
// gateway turn into a signed-out window is refused (TASK-014).

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage } from "./harness/fakeWindow";
import { OctaneChat, settlePage } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

// Codes the page already fails with for other reasons: none of them may
// stand in for "not signed in".
const OTHER_FAILURES = [
  "composer_not_found",
  "send_button_not_found",
  "chat_url_missing",
  "answer_timeout",
  "answer_not_detected",
  "target_not_reached",
  "target_mismatch",
];

// The gateway's window is signed out (octane markup). Before TASK-014 the
// prompt was typed and sent to the anonymous ChatGPT, and the turn failed
// only afterwards with `chat_url_missing`. Now the turn is refused before
// anything is typed, with `not_logged_in`; bridge.ts maps that code to
// `tab_unavailable` (bridgeHost.test.ts, "a signed-out page ...").
test(
  "audit.e4_signed_out_page_gets_no_prompt",
  async () => {
    const chat = new OctaneChat("/");
    try {
      chat.onSend = (prompt) => chat.answer(prompt, "An anonymous answer.");
      const reply = await ask("e4", "project source code …", { target: { kind: "new_chat" } });
      assert.deepEqual(chat.sentPrompts, [], "the prompt reached the signed-out chat");
      assert.equal(chat.submit.clicks, 0);
      assert.equal(chat.composer.value, "", "nothing is typed into the signed-out page");
      assert.equal(reply.ok, false, JSON.stringify(reply));
      assert.ok(!OTHER_FAILURES.includes(reply.errorCode ?? ""), `failed for another reason: ${JSON.stringify(reply)}`);
      assert.equal(reply.errorCode, "not_logged_in");
      assert.equal(reply.message, "ChatGPT в окне cgpt-bridge не выполнил вход — войдите в этом окне");
    } finally {
      await settlePage();
    }
  },
);

// A saved chat of the account never opens in a signed-out window: after the
// page load the turn fails with the sign-in message, not "the target chat
// did not open".
const SAVED_CHAT = "https://chatgpt.com/c/abc-123";

test("audit.e4_saved_chat_in_a_signed_out_window_fails_with_the_sign_in_message", async () => {
  // The load of /c/abc-123 ended on the signed-out start page.
  const chat = new OctaneChat("/");
  try {
    const reply = await ask("e4b", "project source code …", {
      target: { kind: "conversation", url: SAVED_CHAT },
      reloaded: true,
    });
    assert.deepEqual(chat.sentPrompts, []);
    assert.equal(chat.composer.value, "");
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "not_logged_in");
    assert.equal(reply.message, "ChatGPT в окне cgpt-bridge не выполнил вход — войдите в этом окне");
  } finally {
    await settlePage();
  }
});

test("audit.e4_saved_chat_address_shown_signed_out_fails_with_the_sign_in_message", async () => {
  // The load kept the chat's address, but its history never renders.
  const chat = new OctaneChat("/c/abc-123");
  try {
    const reply = await ask("e4c", "project source code …", {
      target: { kind: "conversation", url: SAVED_CHAT },
      reloaded: true,
      deadlineMs: Date.now() + 1_000,
    });
    assert.deepEqual(chat.sentPrompts, []);
    assert.equal(chat.composer.value, "");
    assert.equal(reply.errorCode, "not_logged_in", JSON.stringify(reply));
  } finally {
    await settlePage();
  }
});
