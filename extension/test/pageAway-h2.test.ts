// H2: a CLI ask (no target) sent from a saved chat watches that chat's path
// like a gateway ask does. When the chat disappears (deleted: ChatGPT moves
// the tab to `/`) or the tab goes elsewhere for longer than the grace, the
// ask fails as target_mismatch at once, not at its deadline, and presses
// nothing. A new chat (start at `/`) has no path to watch: unchanged.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, goTo, loadPage } from "./harness/fakeWindow";
import { AccountChat, later, settlePage } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const DEADLINE_MS = 120_000;
const PROJECT_CHAT = "/g/g-p-0123456789abcdef0123456789abcdef-my-proj/c/pc1";

for (const [name, path] of [
  ["a_saved_chat", "/c/abc"],
  ["a_project_chat", PROJECT_CHAT],
] as const) {
  test(`away.cli_ask_in_${name}_ends_within_the_grace_when_the_chat_disappears`, async () => {
    const chat = new AccountChat(path, [{ id: "m0", text: "An earlier answer." }]);
    try {
      let goneAt = 0;
      chat.onSend = () => {
        chat.beginTurn();
        // The chat is deleted: ChatGPT sends the tab to `/`, nothing answers.
        later(100, () => {
          chat.showOtherChat("/", []);
          goneAt = Date.now();
        });
      };
      const reply = await ask("h2-a", "question", { timeoutMs: DEADLINE_MS });
      const after = Date.now() - goneAt;
      assert.equal(reply.ok, false, JSON.stringify(reply));
      assert.equal(reply.errorCode, "target_mismatch");
      assert.match(String(reply.message), /chat the prompt was sent to was closed or deleted/);
      // The prompt is in: point to the chat, never to a retry.
      assert.match(String(reply.message), /open https:\/\/chatgpt\.com\/\S+ to read the answer \(asking again would send it twice\)/);
      assert.ok(after >= 3_000 && after < 4_500, `failed ${after} ms after the chat went`);
      assert.equal(chat.stop.clicks, 0);
    } finally {
      await settlePage();
    }
  });
}

// The tab goes to another saved chat and stays there: the same.
test("away.cli_ask_ends_when_the_tab_stays_in_another_chat", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      chat.beginTurn();
      later(100, () => chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]));
    };
    const reply = await ask("h2-b", "question", { timeoutMs: DEADLINE_MS });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

// A visit shorter than the grace does not fail the ask.
test("away.cli_ask_survives_a_short_absence", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => goTo("/"));
      later(2_000, () => goTo("/c/abc"));
      later(2_500, () => {
        turn.render("Our answer.");
        turn.finish();
      });
    };
    const reply = await ask("h2-c", "question", { timeoutMs: DEADLINE_MS });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "Our answer.");
  } finally {
    await settlePage();
  }
});

// A project may rename itself while its chat is open: the same chat.
test("away.cli_ask_in_a_project_chat_survives_a_slug_change", async () => {
  const chat = new AccountChat(PROJECT_CHAT, [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => goTo("/g/g-p-0123456789abcdef0123456789abcdef-renamed/c/pc1"));
      later(5_000, () => {
        turn.render("Our answer.");
        turn.finish();
      });
    };
    const reply = await ask("h2-d", "question", { timeoutMs: DEADLINE_MS });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "Our answer.");
  } finally {
    await settlePage();
  }
});

// A new chat started at `/` legitimately moves to `/c/<id>`: not watched.
test("away.cli_ask_from_the_start_page_moves_to_its_new_chat", async () => {
  const chat = new AccountChat("/");
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => goTo("/c/new1"));
      later(5_000, () => {
        turn.render("New answer.");
        turn.finish();
      });
    };
    const reply = await ask("h2-e", "question", { timeoutMs: DEADLINE_MS });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "New answer.");
    assert.equal(reply.conversationUrl, "https://chatgpt.com/c/new1");
  } finally {
    await settlePage();
  }
});
