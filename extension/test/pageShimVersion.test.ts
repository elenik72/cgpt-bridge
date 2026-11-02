// A page whose pageShim speaks another protocol version than content.ts: a
// tab kept open across an update keeps the shim it loaded (pageShim.ts wraps
// fetch once per page), so after a future protocol change the two would not
// match. content.ts refuses that shim, says so in one console line, and
// answers from the DOM alone — its reports, epoch and all, are not used.
// Own file: the real shim must stay out of this window (see
// harness/fakeWindow.ts).

import { mock, test } from "node:test";
import assert from "node:assert/strict";

import { ORIGIN, ask, loadPage, win } from "./harness/fakeWindow";
import { AccountChat, later, settlePage } from "./harness/chatPage";
import { PAGE_SHIM_PROTOCOL } from "../src/bridgeProtocol";

const OTHER = PAGE_SHIM_PROTOCOL + 1;

// The real shim installs only once per window; mark it as there, and stand
// in a shim of another version: it acknowledges with an epoch and its `v`.
(win as unknown as Record<string, unknown>).__cgptBridgePageShim = OTHER;
let epoch = 0;
win.addEventListener("message", (ev) => {
  const d = (ev as MessageEvent).data as Record<string, unknown> | null;
  if (!d || d.__cgptBridgeControl !== true) return;
  if (d.active === true) epoch++;
  win.postMessage({ __cgptBridgeAck: true, active: d.active === true, epoch, v: OTHER }, ORIGIN);
});

await loadPage();

const CHAT = "https://chatgpt.com/c/abc";

test("page.a_shim_of_another_protocol_version_is_refused_with_a_line_and_the_dom_answers", async () => {
  const warnings: string[] = [];
  const warn = mock.method(console, "warn", (...args: unknown[]) => void warnings.push(args.map(String).join(" ")));
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      const report = (payload: object): void => win.postMessage({ __cgptBridge: true, epoch, ...payload }, ORIGIN);
      report({ kind: "sse-start", text: "" });
      later(50, () => report({ kind: "sse-done", text: "Text from the other shim" }));
      later(300, () => turn.render("The answer the DOM shows."));
      later(600, () => turn.finish());
    };

    const reply = await ask("v1", "hello", { target: { kind: "conversation", url: CHAT } });

    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "The answer the DOM shows.");
    assert.equal(chat.send.clicks, 1);
    const lines = warnings.filter((w) => w.includes("page shim"));
    assert.ok(lines.length >= 1, warnings.join("\n"));
    assert.match(lines[0]!, new RegExp(`protocol ${OTHER}.*${PAGE_SHIM_PROTOCOL}`));
  } finally {
    warn.mock.restore();
    await settlePage();
  }
});
