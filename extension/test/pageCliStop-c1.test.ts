// C1: a stop naming a running CLI ask (`stop_ask` → content `stop` with
// `askId`, Ctrl+C in `cgpt ask`) presses Stop in the user's own tab only
// while the page shows that ask's chat — the rule a gateway ask's stop has
// (FU-PAGE): the saved chat it was sent from by its path; a new chat by its
// one user turn holding the prompt, whatever its address is by then. Else
// nothing is pressed and the reply says the tab shows another chat.

import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";

import { ask, goTo, h, loadPage, sendToContent } from "./harness/fakeWindow";
import { AccountChat, later, settlePage, type AccountTurn } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const TIMEOUT_MS = 20_000;

function stopAsk(id: string, askId: string): Promise<Record<string, unknown>> {
  return sendToContent({ id, type: "stop", askId });
}

function userBubble(text: string) {
  return h("div", { sels: ["[data-user-message-bubble]"], text });
}

function quietWarnings(t: TestContext): void {
  t.mock.method(console, "warn", () => {});
}

// Runs a CLI ask whose page does `onSend`, sends the stop 1 s after Send,
// and returns the stop's reply once the ask has ended.
async function stopDuringCliAsk(
  chat: AccountChat,
  id: string,
  onSend: (prompt: string) => AccountTurn,
): Promise<Record<string, unknown>> {
  let turn = null as AccountTurn | null;
  chat.onSend = (prompt) => {
    turn = onSend(prompt);
  };
  chat.stop.onClick = () => turn?.finish("cancelled");
  let stopped: Record<string, unknown> | null = null;
  const pending = ask(id, "explain this failure", { timeoutMs: TIMEOUT_MS });
  later(1_000, () => {
    void stopAsk(`${id}-stop`, id).then((r) => (stopped = r));
  });
  await pending;
  assert.ok(stopped !== null, "the stop was answered");
  return stopped;
}

test("cliStop.same_saved_chat_is_stopped", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    const stopped = await stopDuringCliAsk(chat, "c1", () => chat.beginTurn());
    assert.deepEqual(stopped, { id: "c1-stop", ok: true, kind: "stop", wasGenerating: true });
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

test("cliStop.tab_switched_to_another_chat_presses_nothing_and_says_so", async (t) => {
  quietWarnings(t);
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    const stopped = await stopDuringCliAsk(chat, "c2", () => {
      const ours = chat.beginTurn();
      // The user opens a chat of their own, which is generating.
      later(100, () => {
        chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
        chat.beginTurn();
      });
      return ours;
    });
    assert.deepEqual(stopped, {
      id: "c2-stop",
      ok: true,
      kind: "stop",
      wasGenerating: false,
      otherChat: true,
    });
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

test("cliStop.new_chat_before_its_path_is_set_is_stopped", async () => {
  const chat = new AccountChat("/");
  try {
    // ChatGPT has not moved the address yet: still `/`, the one user turn.
    const stopped = await stopDuringCliAsk(chat, "c3", (prompt) => {
      const turn = chat.beginTurn();
      turn.el.append(userBubble(prompt));
      return turn;
    });
    assert.deepEqual(stopped, { id: "c3-stop", ok: true, kind: "stop", wasGenerating: true });
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

test("cliStop.new_chat_after_its_path_is_set_is_stopped", async () => {
  const chat = new AccountChat("/");
  try {
    const stopped = await stopDuringCliAsk(chat, "c4", (prompt) => {
      goTo("/c/local-chatgpt:9c8d");
      const turn = chat.beginTurn();
      turn.el.append(userBubble(prompt));
      later(300, () => goTo("/c/new-4"));
      return turn;
    });
    assert.deepEqual(stopped, { id: "c4-stop", ok: true, kind: "stop", wasGenerating: true });
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

test("cliStop.new_chat_left_for_another_chat_presses_nothing", async (t) => {
  quietWarnings(t);
  const chat = new AccountChat("/");
  try {
    const stopped = await stopDuringCliAsk(chat, "c5", (prompt) => {
      const ours = chat.beginTurn();
      ours.el.append(userBubble(prompt));
      later(100, () => {
        chat.showOtherChat("/c/mine", [{ id: "x1", text: "An answer to someone else." }]);
        chat.beginTurn().el.append(userBubble("A question the user asks now."));
      });
      return ours;
    });
    assert.equal(stopped.otherChat, true, JSON.stringify(stopped));
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

test("cliStop.new_chat_left_for_the_home_page_presses_nothing", async (t) => {
  quietWarnings(t);
  const chat = new AccountChat("/");
  try {
    const stopped = await stopDuringCliAsk(chat, "c6", (prompt) => {
      const ours = chat.beginTurn();
      ours.el.append(userBubble(prompt));
      // A fresh start page, and something there still shows Stop.
      later(100, () => {
        chat.showOtherChat("/", []);
        chat.showStop();
      });
      return ours;
    });
    assert.equal(stopped.otherChat, true, JSON.stringify(stopped));
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

test("cliStop.a_stop_before_send_says_the_prompt_was_not_sent_and_nothing_is_typed", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let sends = 0;
  chat.onSend = () => {
    sends++;
  };
  try {
    const pending = ask("c7", "explain this failure", { timeoutMs: TIMEOUT_MS });
    // In the same turn as the ask: before its first checkpoint.
    const stopped = await stopAsk("c7-stop", "c7");
    const reply = await pending;
    assert.deepEqual(stopped, {
      id: "c7-stop",
      ok: true,
      kind: "stop",
      wasGenerating: false,
      notSent: true,
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal((reply as { errorCode?: string }).errorCode, "cancelled", JSON.stringify(reply));
    assert.equal(sends, 0, "nothing was sent");
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});
