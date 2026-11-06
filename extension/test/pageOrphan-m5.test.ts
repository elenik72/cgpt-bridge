// M5: after an extension reload or update the old content script is cut off
// from the extension but keeps running next to the new one. An ask it is
// still driving must not type or send any more: the new script may be using
// the same composer. It aborts at its checkpoints, before typing and right
// before the Send click.

import { afterEach, test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage, setExtensionContextAlive } from "./harness/fakeWindow";
import { AccountChat, later, settlePage } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const TIMEOUT_MS = 20_000;

afterEach(() => setExtensionContextAlive(true));

test("orphan.a_reload_before_typing_types_and_sends_nothing", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    const pending = ask("o1", "explain this failure", { timeoutMs: TIMEOUT_MS });
    setExtensionContextAlive(false);
    const reply = await pending;
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal((reply as { errorCode?: string }).errorCode, "extension_reloaded", JSON.stringify(reply));
    assert.equal(chat.composerText, "", "nothing typed");
    assert.deepEqual(chat.sentPrompts, []);
  } finally {
    await settlePage();
  }
});

test("orphan.a_reload_while_waiting_for_send_clicks_nothing", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  // Send is not shown yet: the ask types, then waits for it.
  chat.send.remove();
  try {
    const pending = ask("o2", "explain this failure", { timeoutMs: TIMEOUT_MS });
    later(1_000, () => {
      setExtensionContextAlive(false);
      chat.showSend();
    });
    const reply = await pending;
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal((reply as { errorCode?: string }).errorCode, "extension_reloaded", JSON.stringify(reply));
    assert.equal(chat.composerText, "explain this failure", "typed before the reload, never sent");
    assert.deepEqual(chat.sentPrompts, []);
  } finally {
    await settlePage();
  }
});

// Review N2: an orphan whose ask was already sent fails later (here at its
// deadline: the answer never ends). Its abandonTurn must not press Stop:
// the chat is the same session's, where the new script may be answering.
test("orphan.a_sent_ask_that_fails_after_the_reload_presses_no_stop", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      chat.beginTurn();
      later(500, () => setExtensionContextAlive(false));
    };
    const reply = await ask("o3", "explain this failure", {
      timeoutMs: TIMEOUT_MS,
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.deepEqual(chat.sentPrompts, ["explain this failure"]);
    assert.equal(chat.stop.clicks, 0, "the orphan pressed Stop");
  } finally {
    await settlePage();
  }
});
