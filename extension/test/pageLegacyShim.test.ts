// The page with a pageShim injected before the extension was reloaded: the
// open tab keeps the old MAIN-world shim while content.ts is new. That shim
// acknowledges and reports without an `epoch` (the format before TASK-010),
// so content.ts cannot tell its reports from those of an earlier ask's
// stream: it ignores them and the answer comes from the DOM alone. Own file:
// the real shim must stay out of this window (see harness/fakeWindow.ts).

import { test } from "node:test";
import assert from "node:assert/strict";

import { ORIGIN, ask, loadPage, win } from "./harness/fakeWindow";
import { AccountChat, later, settlePage } from "./harness/chatPage";

// The real shim installs only once per window; mark it as there, and stand
// in the old one: switch on/off, acknowledged without an epoch.
(win as unknown as Record<string, unknown>).__cgptBridgePageShim = true;
win.addEventListener("message", (ev) => {
  const d = (ev as MessageEvent).data as Record<string, unknown> | null;
  if (!d || d.__cgptBridgeControl !== true) return;
  win.postMessage({ __cgptBridgeAck: true, active: d.active === true }, ORIGIN);
});

await loadPage();

const CHAT = "https://chatgpt.com/c/abc";

const oldShimReports = (payload: object): void => win.postMessage({ __cgptBridge: true, ...payload }, ORIGIN);

test("page.reports_of_a_shim_without_epoch_are_ignored_and_the_dom_answers", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      oldShimReports({ kind: "sse-start", text: "" });
      later(50, () => {
        oldShimReports({ kind: "sse-tokens", text: "Text from **a stream**" });
        oldShimReports({ kind: "sse-done", text: "Text from **a stream**" });
      });
      later(300, () => turn.render("The answer the DOM shows."));
      later(600, () => turn.finish());
    };

    const reply = await ask("l1", "hello", { target: { kind: "conversation", url: CHAT } });

    assert.deepEqual(reply, {
      id: "l1",
      ok: true,
      kind: "test.ask",
      text: "The answer the DOM shows.",
      conversationUrl: CHAT,
    });
    assert.deepEqual(chat.sentPrompts, ["hello"]);
    assert.equal(chat.send.clicks, 1);
  } finally {
    await settlePage();
  }
});
