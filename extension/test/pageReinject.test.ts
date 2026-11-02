// content.ts injected into a page that already runs it: on install or
// update (src/reinject.ts), or by the background after "no receiver", while
// the manifest injects it too. A second run of the file in the same isolated
// world must not add a second message listener (each request would then be
// handled twice: two prompts typed, two Send clicks). The guard is a flag on
// that world's `window`, which a second run of the bundle shares (module
// state does not survive it). Own file: one window per file (see
// harness/fakeWindow.ts).

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage, runtimeListenerCount, win } from "./harness/fakeWindow";
import { PAGE_SHIM_PROTOCOL } from "../src/bridgeProtocol";
import { AccountChat, later, settlePage } from "./harness/chatPage";

await loadPage();
const { installContentScript } = await import("../src/content");

test("page.content_script_run_again_in_the_same_page_adds_no_second_listener", async () => {
  assert.equal(runtimeListenerCount(), 1);
  assert.equal(installContentScript(), false);
  assert.equal(installContentScript(), false);
  assert.equal(runtimeListenerCount(), 1);

  // The one listener still answers, once.
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => turn.render("The answer."));
      later(300, () => turn.finish());
    };
    const reply = await ask("r1", "hello", { target: { kind: "conversation", url: "https://chatgpt.com/c/abc" } });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "The answer.");
    assert.deepEqual(chat.sentPrompts, ["hello"]);
    assert.equal(chat.send.clicks, 1);
    // The shim marks the page, and acknowledges, with its protocol version.
    assert.equal((win as unknown as Record<string, unknown>).__cgptBridgePageShim, PAGE_SHIM_PROTOCOL);
    const ack = win.posted.find((d) => (d as Record<string, unknown> | null)?.__cgptBridgeAck === true);
    assert.equal((ack as Record<string, unknown>).v, PAGE_SHIM_PROTOCOL);
  } finally {
    await settlePage();
  }
});

test("page.the_guard_is_the_window_flag_not_module_state", () => {
  // What a fresh run of the bundle sees: its own module state, the page's
  // window. Without the flag it installs (a reloaded extension's new
  // isolated world has none).
  const w = globalThis as unknown as Record<string, unknown>;
  const flag = Object.keys(w).find((k) => k.startsWith("__cgptBridgeContent"));
  assert.ok(flag, "content.ts marks the window");
  delete w[flag];
  assert.equal(installContentScript(), true);
  assert.equal(runtimeListenerCount(), 2);
});
