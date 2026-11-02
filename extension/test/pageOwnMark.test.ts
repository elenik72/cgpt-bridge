// The own-window mark in the page (TASK-054). A gateway ask (it names a
// chat) writes the mark the background gives it into the page's
// sessionStorage, which outlives an extension reload and comes back with a
// window Chrome restores. After a reload the background asks the page for it
// (`own_mark`) to know its own window again. A CLI ask never writes it.
// Own file: one window per file (see harness/fakeWindow.ts).

import { mock, test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage, sendToContent, sessionStorageThrows, sessionStore } from "./harness/fakeWindow";
import { AccountChat, later, settlePage } from "./harness/chatPage";

await loadPage();

const MARK = "3f0c9a52-6d1e-4b7a-9c2f-0e8d5a4b1c77";
const CHAT = "https://chatgpt.com/c/abc";

interface MarkReply {
  id: string;
  ok: boolean;
  kind?: string;
  mark?: string | null;
}
const readMark = (): Promise<MarkReply> => sendToContent<MarkReply>({ id: "m1", type: "own_mark" });

async function answeredAsk(extra: Record<string, unknown>): Promise<void> {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => turn.render("The answer."));
      later(300, () => turn.finish());
    };
    const reply = await ask("r1", "hello", extra);
    assert.equal(reply.ok, true, JSON.stringify(reply));
  } finally {
    await settlePage();
  }
}

test("page.a_page_without_the_mark_reports_none", async () => {
  sessionStore.clear();
  assert.deepEqual(await readMark(), { id: "m1", ok: true, kind: "own_mark", mark: null });
});

test("page.a_gateway_ask_writes_the_mark_it_carries_and_the_page_reports_it", async () => {
  sessionStore.clear();
  await answeredAsk({ target: { kind: "conversation", url: CHAT }, ownMark: MARK });
  assert.deepEqual(await readMark(), { id: "m1", ok: true, kind: "own_mark", mark: MARK });
  // Only the mark goes into the page's storage.
  assert.deepEqual([...sessionStore.values()], [MARK]);
});

test("page.a_cli_ask_never_writes_a_mark", async () => {
  sessionStore.clear();
  await answeredAsk({ ownMark: MARK });
  await answeredAsk({ ownMark: 42, target: { kind: "conversation", url: CHAT } });
  assert.equal((await readMark()).mark, null);
  assert.equal(sessionStore.size, 0);
});

test("page.a_storage_that_throws_reports_no_mark_and_the_ask_still_runs", async () => {
  sessionStore.clear();
  sessionStorageThrows.on = true;
  const warnings: string[] = [];
  const warn = mock.method(console, "warn", (...args: unknown[]) => void warnings.push(args.map(String).join(" ")));
  try {
    await answeredAsk({ target: { kind: "conversation", url: CHAT }, ownMark: MARK });
    assert.equal((await readMark()).mark, null);
  } finally {
    sessionStorageThrows.on = false;
    warn.mock.restore();
  }
  assert.ok(warnings.some((w) => w.includes("could not mark the gateway window's page")), warnings.join("\n"));
  assert.equal(sessionStore.size, 0);
});
