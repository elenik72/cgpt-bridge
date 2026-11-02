// The gateway's own window after an extension reload or update, or a browser
// restart (TASK-054), end to end on fakes: chrome.storage.session is empty,
// the window's ids and mark are in chrome.storage.local, and the page reports
// its mark when asked (`own_mark`), with the page scripts injected first when
// no content script answers, as after a reload. The fake `chrome` cannot
// open a window, so an ask that does not take the marked tab back ends in
// `tab_unavailable`. Own file: the bridge's chooser keeps what it found for
// the life of the module, so the tests run in this order.
// Run with `npm run test` (esbuild bundle + node:test, no browser).

import { after, before, beforeEach, mock, test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

// bridge.ts reads the `chrome` global when it loads; the fake goes first.
import { resetWorld, useTabs, world } from "./harness/fakeChrome";
import { noteBrowserStart, startBridge } from "../src/bridge";
import { MARK_READ_TIMEOUT_MS } from "../src/targetTab";
import type { ExtensionRequest, ExtensionResponse, TestAskRequest } from "../src/bridgeProtocol";
import { advance } from "./harness/clock";
import { fakeTabs, type FakeTabs } from "./harness/fakeTabs";

const NOW = 1_700_000_000_000;
const TAB_ID = 7;
const WINDOW_ID = 1;
const CHAT_URL = "https://chatgpt.com/c/abc-123";
const MARK = "3f0c9a52-6d1e-4b7a-9c2f-0e8d5a4b1c77";
const NO_LISTENER = "Could not establish connection. Receiving end does not exist.";

const FIXTURES = new URL("../../protocol/tests/fixtures/", import.meta.url);
function fixture(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(new URL(`${name}.json`, FIXTURES), "utf8"));
}

before(() => {
  mock.timers.enable({ apis: ["setTimeout", "setInterval", "Date"], now: NOW });
  mock.method(console, "log", () => {});
  mock.method(console, "info", () => {});
  startBridge();
});
after(() => {
  mock.timers.reset();
});

let tabs: FakeTabs;
let pageMark: string | null;
beforeEach(() => {
  resetWorld();
  tabs = fakeTabs({ id: TAB_ID, status: "complete", url: CHAT_URL });
  (tabs.tab as { windowId?: number }).windowId = WINDOW_ID;
  useTabs(tabs);
  // The one open tab, alone in its (normal) window.
  (chrome.tabs as unknown as { query: () => Promise<unknown[]> }).query = async () => [
    { ...tabs.tab, active: true },
  ];
  pageMark = MARK;
  world.content = async (_tabId, req) => pageReply(req);
});

function pageReply(req: ExtensionRequest): ExtensionResponse {
  if (req.type === "own_mark") return { id: req.id, ok: true, kind: "own_mark", mark: pageMark };
  if (req.type === "test.ask") {
    return { id: req.id, ok: true, kind: "test.ask", text: "the answer", conversationUrl: CHAT_URL };
  }
  return { id: req.id, ok: false, errorCode: "bad_request", message: "unexpected" };
}

function receive(msg: unknown): void {
  assert.ok(world.hostMessage, "the bridge opened its native port");
  world.hostMessage(msg);
}
async function until(done: () => boolean, maxMs = 200_000): Promise<void> {
  for (let waited = 0; !done() && waited < maxMs; waited += 100) await advance(100, 100);
  assert.ok(done(), "condition was not reached");
}
const posted = (id: string): Array<Record<string, unknown>> =>
  world.posted.filter((f) => f.id === id);
const askSends = (): TestAskRequest[] =>
  world.sent.filter((s) => s.req.type === "test.ask").map((s) => s.req as TestAskRequest);

test("ownWindow.after_a_reload_the_gateway_ask_runs_in_the_marked_window", async () => {
  world.local.ownWindowMark = { token: MARK, windowId: WINDOW_ID, tabId: TAB_ID };
  // The content script of the old extension no longer answers: the page
  // scripts go in (R1), then the page reports its mark.
  let first = true;
  world.content = async (_tabId, req) => {
    if (req.type === "own_mark" && first) {
      first = false;
      return { lastError: NO_LISTENER };
    }
    return pageReply(req);
  };
  receive(fixture("request_ask_conversation"));
  await until(() => posted("a2").length > 0);

  assert.equal(posted("a2")[0]?.type, "ask_result", JSON.stringify(posted("a2")));
  assert.deepEqual(
    world.injected.map((i) => i.file),
    ["pageShim.js", "content.js"],
  );
  assert.deepEqual(world.session.ownTab, { windowId: WINDOW_ID, tabId: TAB_ID });
  assert.ok(tabs.updates.some((u) => u.props.autoDiscardable === false));
  // The ask keeps the page marked.
  assert.equal(askSends()[0]?.ownMark, MARK);
  assert.deepEqual(world.sent.map((s) => [s.tabId, s.req.type]), [
    [TAB_ID, "own_mark"],
    [TAB_ID, "own_mark"],
    [TAB_ID, "test.ask"],
  ]);
});

test("ownWindow.after_a_reload_the_cli_ask_does_not_take_the_marked_window", async () => {
  world.local.ownWindowMark = { token: MARK, windowId: WINDOW_ID, tabId: TAB_ID };
  receive(fixture("request_ask"));
  await until(() => posted("a1").length > 0);

  assert.equal(posted("a1")[0]?.code, "tab_unavailable");
  assert.match(String(posted("a1")[0]?.message), /no https:\/\/chatgpt\.com\/ tab is open/);
  assert.deepEqual(askSends(), []);
});

test("ownWindow.after_a_browser_restart_the_marked_window_is_taken_back_only_after_onStartup", async () => {
  // Chrome restored the window under new ids; the record has the old ones.
  // An ask before `onStartup` cannot tell a restart from a reload: the
  // marked tab with other ids stays ordinary and the record goes.
  world.local.ownWindowMark = { token: MARK, windowId: 9, tabId: 3 };
  receive(fixture("request_ask_conversation"));
  await until(() => posted("a2").length > 0);
  assert.equal(posted("a2")[0]?.code, "tab_unavailable");
  assert.match(String(posted("a2")[0]?.message), /no window may be opened/);
  assert.deepEqual(askSends(), []);
  assert.equal(world.local.ownWindowMark, undefined);

  // After `onStartup` it is taken back, and the record follows the new ids.
  world.posted = [];
  world.local.ownWindowMark = { token: MARK, windowId: 9, tabId: 3 };
  await noteBrowserStart();
  receive(fixture("request_ask_conversation"));
  await until(() => posted("a2").length > 0);
  assert.equal(posted("a2")[0]?.type, "ask_result", JSON.stringify(posted("a2")));
  assert.deepEqual(world.local.ownWindowMark, { token: MARK, windowId: WINDOW_ID, tabId: TAB_ID });
  assert.equal(askSends()[0]?.ownMark, MARK);
});

test("ownWindow.a_page_that_never_answers_holds_up_no_request_past_the_bound", async () => {
  // A frozen tab, or one behind a modal dialog: the message never returns.
  world.local.ownWindowMark = { token: MARK, windowId: WINDOW_ID, tabId: TAB_ID };
  world.content = (_tabId, req) =>
    req.type === "own_mark" ? new Promise(() => {}) : Promise.resolve(pageReply(req));
  const bound = 2 * MARK_READ_TIMEOUT_MS;
  const start = Date.now();
  receive(fixture("request_status"));
  receive(fixture("request_ask"));
  await until(() => world.posted.length >= 2, bound);
  assert.ok(Date.now() - start <= bound, `answered after ${Date.now() - start} ms`);

  // Status: no own tab known.
  const status = world.posted.find((f) => f.type === "status_result");
  assert.equal(status?.tab_open, false, JSON.stringify(world.posted));
  // The CLI does not take the tab that may be the own one.
  assert.equal(posted("a1")[0]?.code, "tab_unavailable");
  assert.deepEqual(askSends(), []);
  // Still in doubt, not given up: the record stays.
  assert.ok(world.local.ownWindowMark);
});

test("ownWindow.a_page_without_the_mark_is_left_alone_and_a_new_window_is_asked_for", async () => {
  world.local.ownWindowMark = { token: MARK, windowId: WINDOW_ID, tabId: TAB_ID };
  pageMark = null;
  receive(fixture("request_ask_conversation"));
  await until(() => posted("a2").length > 0);

  assert.equal(posted("a2")[0]?.code, "tab_unavailable");
  assert.match(String(posted("a2")[0]?.message), /no window may be opened/);
  assert.deepEqual(askSends(), []);
  assert.equal(world.session.ownTab, undefined);
  assert.deepEqual(tabs.updates, []);
});
