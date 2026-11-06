// The gateway window's tab pool (T7) end to end on fakes: host frames go
// through the real routing, tab choice, queue and send loop against a fake
// `chrome` with several tabs, and the frames posted back are checked. Own
// file: the bridge keeps its chooser and the tab of the last gateway ask for
// the life of the module, so the tests run in this order.
// Run with `npm run test` (esbuild bundle + node:test, no browser).

import { after, before, beforeEach, mock, test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

// bridge.ts reads the `chrome` global when it loads; the fake goes first.
import { resetWorld, useTabs, world, type ContentReply } from "./harness/fakeChrome";
import { ENDED_ASK_MEMORY_MS, startBridge } from "../src/bridge";
import { OWN_TABS_MAX } from "../src/targetTab";
import {
  errorFrame,
  type ExtensionRequest,
  type ExtensionResponse,
  type TestAskRequest,
} from "../src/bridgeProtocol";
import { advance } from "./harness/clock";
import type { FakeTabs } from "./harness/fakeTabs";

const NOW = 1_700_000_000_000;
const WINDOW_ID = 1;
// request_ask_conversation targets this chat.
const CHAT_URL = "https://chatgpt.com/c/abc-123";

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

interface PoolTab {
  id: number;
  windowId: number;
  url: string;
  status: string;
}

// `chrome.tabs` with several tabs: get / update / create / query, and
// lifecycle listeners that never fire (every tab is loaded).
function poolTabs(open: PoolTab[]) {
  const tabs = [...open];
  const created: chrome.tabs.CreateProperties[] = [];
  let nextId = 100;
  const listeners = { addListener: () => {}, removeListener: () => {} };
  const api = {
    async get(tabId: number) {
      const tab = tabs.find((t) => t.id === tabId);
      if (!tab) throw new Error(`No tab with id: ${tabId}.`);
      return { ...tab } as unknown as chrome.tabs.Tab;
    },
    async update(tabId: number) {
      return { ...tabs.find((t) => t.id === tabId) } as unknown as chrome.tabs.Tab;
    },
    onUpdated: listeners,
    onRemoved: listeners,
  };
  useTabs({ api: { tabs: api } } as unknown as FakeTabs);
  Object.assign(chrome.tabs, {
    async create(props: chrome.tabs.CreateProperties) {
      created.push(props);
      const tab: PoolTab = {
        id: nextId++,
        windowId: props.windowId as number,
        url: String(props.url),
        status: "complete",
      };
      tabs.push(tab);
      return { ...tab };
    },
    async query() {
      return tabs.map((t) => ({ ...t }));
    },
  });
  return { tabs, created };
}

const tab = (id: number, url = "https://chatgpt.com/"): PoolTab => ({
  id,
  windowId: WINDOW_ID,
  url,
  status: "complete",
});

beforeEach(() => {
  resetWorld();
  // A host that reported no build (an older one): stopped asks are `internal`.
  const quiet = mock.method(console, "warn", () => {});
  try {
    world.hostMessage?.(fixture("legacy/response_pong"));
  } finally {
    quiet.mock.restore();
  }
});

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
const sends = (type: string): Array<{ tabId: number; req: ExtensionRequest }> =>
  world.sent.filter((s) => s.req.type === type);

function gate(): { promise: Promise<ContentReply>; release: (r: ContentReply) => void } {
  let release!: (r: ContentReply) => void;
  const promise = new Promise<ContentReply>((r) => (release = r));
  return { promise, release };
}
const answer = (req: ExtensionRequest): ExtensionResponse => ({
  id: req.id,
  ok: true,
  kind: "test.ask",
  text: "the answer",
  conversationUrl: CHAT_URL,
});
const cancelled = (req: ExtensionRequest): ExtensionResponse => ({
  id: req.id,
  ok: false,
  errorCode: "cancelled",
  message: "cancelled by stop",
});

test("pool.status_answers_on_a_pool_tab_with_no_ask_and_says_how_many_tabs_the_window_keeps", async () => {
  poolTabs([tab(7, CHAT_URL), tab(8, "https://chatgpt.com/c/other")]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [7, 8] };
  const held = gate();
  world.content = (_tabId, req) => {
    if (req.type === "test.ask") return held.promise;
    if (req.type === "status") {
      return Promise.resolve({
        id: req.id,
        ok: true,
        kind: "status",
        loggedIn: true,
        conversationUrl: "https://chatgpt.com/c/other",
      });
    }
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation")); // a2 runs in 7, which shows its chat
  await until(() => sends("test.ask").length === 1);
  assert.equal(sends("test.ask")[0]?.tabId, 7);

  receive(fixture("request_status"));
  await until(() => posted("t1").length > 0);
  assert.deepEqual(
    sends("status").map((s) => s.tabId),
    [8],
  );
  const status = posted("t1")[0]!;
  assert.equal(status.tab_open, true);
  assert.equal(status.logged_in, true);
  assert.equal(status.own_tabs, OWN_TABS_MAX);
  assert.equal(status.own_tabs, 8);

  held.release(answer(sends("test.ask")[0]!.req));
  await until(() => posted("a2").length > 0);
  assert.equal(posted("a2")[0]?.type, "ask_result");
});

test("pool.two_gateway_asks_run_at_once_and_a_plain_stop_stops_both", async () => {
  const fake = poolTabs([tab(7, CHAT_URL)]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [7] };
  const held = new Map<number, ReturnType<typeof gate>>();
  world.content = (tabId, req) => {
    if (req.type === "test.ask") {
      const g = gate();
      held.set(tabId, g);
      return g.promise;
    }
    if (req.type === "stop") {
      // Only the answer in the old tab is still generating.
      return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: tabId === 7 });
    }
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation")); // a2
  receive(fixture("request_ask_new_chat")); // a3
  await until(() => sends("test.ask").length === 2);

  // a3 did not wait for a2: a second tab opened in the own window.
  const [first, second] = sends("test.ask");
  assert.equal(first?.tabId, 7);
  assert.equal(second?.tabId, 100);
  assert.deepEqual(fake.created, [{ windowId: WINDOW_ID, url: "https://chatgpt.com/", active: false }]);
  assert.deepEqual(world.session.ownTab, { windowId: WINDOW_ID, tabIds: [7, 100] });

  // An older gateway's stop: every gateway ask, each in its own tab.
  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);
  assert.deepEqual(
    sends("stop").map((s) => [s.tabId, (s.req as { askId?: string }).askId]),
    [
      [7, (first!.req as TestAskRequest).id],
      [100, (second!.req as TestAskRequest).id],
    ],
  );
  assert.deepEqual(posted("s1"), [{ type: "stopped", id: "s1", was_generating: true }]);

  held.get(7)!.release(cancelled(first!.req));
  held.get(100)!.release(cancelled(second!.req));
  await until(() => posted("a2").length > 0 && posted("a3").length > 0);
  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
  assert.deepEqual(posted("a3"), [errorFrame("a3", "internal", "cancelled by stop")]);
});

test("pool.a_stop_with_no_ask_in_flight_presses_stop_in_the_tab_of_the_last_gateway_ask", async () => {
  poolTabs([tab(7, "https://chatgpt.com/c/other"), tab(8, CHAT_URL)]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [7, 8] };
  world.content = async (_tabId, req) => {
    if (req.type === "test.ask") return answer(req);
    if (req.type === "stop") return { id: req.id, ok: true, kind: "stop", wasGenerating: true };
    return { lastError: "unexpected" };
  };
  receive(fixture("request_ask_conversation")); // a2 runs in 8, which shows its chat
  await until(() => posted("a2").length > 0);
  assert.deepEqual(
    sends("test.ask").map((s) => s.tabId),
    [8],
  );

  // It ended (a timeout, say) while its answer may still be generating.
  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);
  assert.deepEqual(
    sends("stop").map((s) => [s.tabId, (s.req as { askId?: string }).askId]),
    [[8, undefined]],
  );
  assert.deepEqual(posted("s1"), [{ type: "stopped", id: "s1", was_generating: true }]);
});

test("pool.a_pool_tab_on_a_sign_in_page_gets_no_status_and_no_cli_ask", async () => {
  const fake = poolTabs([
    tab(7, "https://auth.openai.com/log-in"),
    tab(8, CHAT_URL),
    { id: 3, windowId: 2, url: "https://chatgpt.com/c/user-chat", status: "complete" },
  ]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [7, 8] };
  world.content = async (_tabId, req) =>
    req.type === "status"
      ? { id: req.id, ok: true, kind: "status", loggedIn: true, conversationUrl: CHAT_URL }
      : answer(req);
  receive(fixture("request_status"));
  receive(fixture("request_ask"));
  await until(() => posted("t1").length > 0 && posted("a1").length > 0);
  assert.deepEqual(
    sends("status").map((s) => s.tabId),
    [8],
  );
  assert.deepEqual(
    sends("test.ask").map((s) => s.tabId),
    [3],
  );

  // Back on chatgpt.com, and the user's tab gone: still not the CLI's.
  fake.tabs[0]!.url = "https://chatgpt.com/";
  fake.tabs.splice(2, 1);
  world.sent = [];
  receive({ ...fixture("request_ask"), id: "a1b" });
  await until(() => posted("a1b").length > 0);
  assert.equal(posted("a1b")[0]?.code, "tab_unavailable");
  assert.deepEqual(sends("test.ask"), []);
});

test("pool.a_cli_ask_never_runs_in_a_pool_tab", async () => {
  // The only ChatGPT tabs are the pool's: the CLI is refused, nothing sent.
  poolTabs([tab(7, CHAT_URL), tab(8)]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [7, 8] };
  world.content = async (_tabId, req) => answer(req);
  receive(fixture("request_ask"));
  await until(() => posted("a1").length > 0);
  assert.equal(posted("a1")[0]?.code, "tab_unavailable");
  assert.deepEqual(sends("test.ask"), []);
});

// ─── stop_ask for a gateway ask that has ended ─────────────────────────

const timedOut = (req: ExtensionRequest): ExtensionResponse => ({
  id: req.id,
  ok: false,
  errorCode: "answer_timeout",
  message: "no answer in time",
});
const stopReplies = (generating: boolean) => (tabId: number, req: ExtensionRequest) =>
  Promise.resolve<ContentReply>(
    req.type === "test.ask"
      ? timedOut(req)
      : req.type === "stop"
        ? { id: req.id, ok: true, kind: "stop", wasGenerating: generating && tabId > 0 }
        : { lastError: "unexpected" },
  );
const stopsSent = (): Array<[number, string | undefined]> =>
  sends("stop").map((s) => [s.tabId, (s.req as { askId?: string }).askId]);

test("pool.stop_ask_after_an_own_ask_timed_out_presses_stop_in_its_tab", async () => {
  poolTabs([tab(7, "https://chatgpt.com/c/other"), tab(8, CHAT_URL)]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [7, 8] };
  world.content = stopReplies(true);
  receive(fixture("request_ask_conversation")); // a2 in 8, times out there
  await until(() => posted("a2").length > 0);
  assert.equal(posted("a2")[0]?.code, "timeout");

  receive({ type: "stop_ask", id: "s2", ask_id: "a2" });
  await until(() => posted("s2").length > 0);
  // The page has closed the ask: the stop names none and presses only if
  // its last turn, a2's, still generates.
  assert.deepEqual(stopsSent(), [[8, undefined]]);
  assert.deepEqual(posted("s2"), [{ type: "stopped", id: "s2", was_generating: true }]);
});

test("pool.stop_ask_for_an_own_ask_that_ended_over_5_minutes_ago_touches_nothing", async () => {
  poolTabs([tab(8, CHAT_URL)]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [8] };
  world.content = stopReplies(true);
  receive({ ...fixture("request_ask_conversation"), id: "a6" });
  await until(() => posted("a6").length > 0);
  await advance(ENDED_ASK_MEMORY_MS);

  receive({ type: "stop_ask", id: "s2", ask_id: "a6" });
  await until(() => posted("s2").length > 0);
  assert.deepEqual(stopsSent(), []);
  assert.deepEqual(posted("s2"), [{ type: "stopped", id: "s2", was_generating: false }]);
});

test("pool.stop_ask_for_an_ended_cli_ask_touches_nothing", async () => {
  poolTabs([
    tab(8, CHAT_URL),
    { id: 3, windowId: 2, url: "https://chatgpt.com/c/user-chat", status: "complete" },
  ]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [8] };
  world.content = stopReplies(true);
  receive({ ...fixture("request_ask"), id: "c1" });
  await until(() => posted("c1").length > 0);
  assert.deepEqual(
    sends("test.ask").map((s) => s.tabId),
    [3],
  );

  receive({ type: "stop_ask", id: "s2", ask_id: "c1" });
  await until(() => posted("s2").length > 0);
  assert.deepEqual(stopsSent(), []);
  assert.deepEqual(posted("s2"), [{ type: "stopped", id: "s2", was_generating: false }]);
});

test("pool.stop_ask_for_an_ended_own_ask_leaves_a_tab_no_longer_in_the_pool_or_used_since", async () => {
  const fake = poolTabs([tab(7, "https://chatgpt.com/c/other"), tab(8, CHAT_URL)]);
  world.session.ownTab = { windowId: WINDOW_ID, tabIds: [7, 8] };
  world.content = stopReplies(true);
  receive({ ...fixture("request_ask_conversation"), id: "a7" }); // in 8
  await until(() => posted("a7").length > 0);
  // 8 moved out of the own window.
  fake.tabs[1]!.windowId = 2;
  receive({ type: "stop_ask", id: "s2", ask_id: "a7" });
  await until(() => posted("s2").length > 0);
  assert.deepEqual(stopsSent(), []);
  assert.deepEqual(posted("s2"), [{ type: "stopped", id: "s2", was_generating: false }]);

  // Another ask has used the tab since: its answer is not a7's.
  receive({ ...fixture("request_ask_conversation"), id: "a8", target: { kind: "conversation", url: "https://chatgpt.com/c/other" } }); // in 7
  await until(() => posted("a8").length > 0);
  receive({ ...fixture("request_ask_conversation"), id: "a9", target: { kind: "new_chat" } }); // 7 again
  await until(() => posted("a9").length > 0);
  assert.deepEqual(
    sends("test.ask").map((s) => s.tabId),
    [8, 7, 7],
  );
  receive({ type: "stop_ask", id: "s3", ask_id: "a8" });
  await until(() => posted("s3").length > 0);
  assert.deepEqual(stopsSent(), []);
  assert.deepEqual(posted("s3"), [{ type: "stopped", id: "s3", was_generating: false }]);
});
