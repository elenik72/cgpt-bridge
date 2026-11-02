// The send loop of a targeted ask in the extension's own tab (askOwnTab /
// sendWhenLoaded in src/bridge.ts), run against a fake tab and a fake content
// script: waiting for the page to load, re-sending only while no content
// script listens yet, the single reload for a "navigate" answer, and `stop`.
// What it pins is that a prompt is never sent twice. Timers and the clock are
// mocked, so the 60 s load wait and the 20 retries run instantly.
// Run with `npm run test` (esbuild bundle + node:test, no browser).

import { afterEach, beforeEach, mock, test } from "node:test";
import assert from "node:assert/strict";

// bridge.ts reads the `chrome` global when it loads; the fake goes first.
import "./harness/fakeChrome";
import { askOwnTab, sendWhenLoaded, type AskDeps } from "../src/bridge";
import type { ActiveAsk } from "../src/askControl";
import type {
  ExtensionRequest,
  ExtensionResponse,
  StatusOkResponse,
  TestAskRequest,
} from "../src/bridgeProtocol";
import { advance } from "./harness/clock";
import { fakeTabs, type FakeTabs } from "./harness/fakeTabs";

const NOW = 1_700_000_000_000;
const TAB_ID = 7;
const TARGET_URL = "https://chatgpt.com/c/abc-123";

beforeEach(() => {
  mock.timers.enable({ apis: ["setTimeout", "setInterval", "Date"], now: NOW });
});
afterEach(() => {
  mock.timers.reset();
});

function askReq(over: Partial<TestAskRequest> = {}): TestAskRequest {
  return {
    id: "req_1",
    type: "test.ask",
    text: "hello",
    timeoutMs: 600_000,
    deadlineMs: NOW + 600_000,
    target: { kind: "conversation", url: TARGET_URL },
    ...over,
  };
}

function active(over: Partial<ActiveAsk> = {}): ActiveAsk {
  return { tabId: TAB_ID, hostId: "a1", own: true, contentRequestId: "req_1", cancelled: false, handedToTab: true, ...over };
}

const okAnswer = (id = "req_1"): ExtensionResponse => ({
  id,
  ok: true,
  kind: "test.ask",
  text: "the answer",
  conversationUrl: TARGET_URL,
});
const navigate = (id = "req_1"): ExtensionResponse => ({
  id,
  ok: true,
  kind: "navigate",
  url: TARGET_URL,
});
const failure = (errorCode: string, id = "req_1"): ExtensionResponse => ({
  id,
  ok: false,
  errorCode,
  message: `fake ${errorCode}`,
});

// A fake content script: answers each send from `script` in order (the last
// entry repeats) and logs what it was sent.
function fakeContent(script: Array<ExtensionResponse | (() => ExtensionResponse)>) {
  const sent: Array<{ tabId: number; req: ExtensionRequest }> = [];
  const diagnoses: number[] = [];
  const sendToTab = async (tabId: number, req: ExtensionRequest): Promise<ExtensionResponse> => {
    if (req.type === "diagnose") {
      diagnoses.push(tabId);
      return new Promise<ExtensionResponse>(() => {}); // the page never answers
    }
    sent.push({ tabId, req });
    const next = script[Math.min(sent.length - 1, script.length - 1)];
    if (next === undefined) throw new Error("fakeContent: empty script");
    return typeof next === "function" ? next() : next;
  };
  return { sent, diagnoses, sendToTab };
}

function depsOf(tabs: FakeTabs, sendToTab: AskDeps["sendToTab"]): AskDeps {
  return { api: tabs.api, sendToTab };
}

// A tab that loads `TARGET_URL` when `tabs.update` is called: loading, then
// complete a moment later.
function loadsOnUpdate(tabs: FakeTabs): void {
  tabs.onUpdate = (props) => {
    tabs.tab.status = "loading";
    tabs.emitUpdated({ status: "loading" });
    setTimeout(() => {
      tabs.tab.url = String(props.url);
      tabs.tab.status = "complete";
      tabs.emitUpdated({ status: "complete" });
    }, 500);
  };
}

// ─── waiting for the page ───────────────────────────────────────────────

test("bridgeAsk.loaded_tab_is_asked_once_with_the_request_unchanged", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const resp = await sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);
  assert.deepEqual(resp, okAnswer());
  assert.deepEqual(content.sent, [{ tabId: TAB_ID, req }]);
  assert.equal(tabs.listenerCount(), 0, "load listeners are removed");
});

test("bridgeAsk.loading_tab_is_not_asked_until_it_completes", async () => {
  const tabs = fakeTabs({ status: "loading" });
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);

  await advance(500);
  assert.equal(content.sent.length, 0, "nothing is sent into a loading tab");

  tabs.tab.status = "complete";
  tabs.emitUpdated({ status: "complete" });
  assert.deepEqual(await run, okAnswer());
  assert.equal(content.sent.length, 1);
  assert.equal(tabs.listenerCount(), 0);
});

test("bridgeAsk.a_missed_complete_event_is_caught_by_the_status_poll", async () => {
  const tabs = fakeTabs({ status: "loading" });
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);

  await advance(500);
  tabs.tab.status = "complete"; // no event reaches the background
  await advance(1_000); // LOAD_POLL_MS
  assert.deepEqual(await run, okAnswer());
  assert.equal(content.sent.length, 1);
});

test("bridgeAsk.a_tab_that_never_loads_times_out_without_sending", async () => {
  const tabs = fakeTabs({ status: "loading", url: "https://chatgpt.com/c/slow" });
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);

  await advance(60_000); // PAGE_LOAD_TIMEOUT_MS
  await advance(2_000); // DIAGNOSE_TIMEOUT_MS: the page does not answer `diagnose`
  const resp = await run;

  assert.equal(resp.ok, false);
  if (resp.ok) return;
  assert.equal(resp.id, "req_1");
  assert.equal(resp.errorCode, "content_script_not_ready");
  assert.match(resp.message, /did not load within 60000 ms/);
  assert.match(resp.message, /waiting for the page the tab was loading/);
  assert.match(resp.message, /tab status=loading url=https:\/\/chatgpt\.com\/c\/slow/);
  assert.match(resp.message, /page: no answer/);
  assert.deepEqual(content.diagnoses, [TAB_ID]);
  assert.equal(content.sent.length, 0, "the prompt is never sent");
  assert.equal(tabs.listenerCount(), 0);
});

test("bridgeAsk.a_deadline_before_the_load_limit_is_an_answer_timeout", async () => {
  const tabs = fakeTabs({ status: "loading" });
  const content = fakeContent([okAnswer()]);
  const req = askReq({ deadlineMs: NOW + 5_000 });
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);

  await advance(5_000);
  const resp = await run;
  assert.equal(resp.ok, false);
  if (resp.ok) return;
  assert.equal(resp.errorCode, "answer_timeout");
  assert.match(resp.message, /deadline passed while the ChatGPT tab was loading/);
  assert.equal(content.sent.length, 0);
  assert.equal(content.diagnoses.length, 0, "no diagnose past the deadline");
});

test("bridgeAsk.a_tab_closed_while_loading_fails_without_sending", async () => {
  const tabs = fakeTabs({ status: "loading" });
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);

  await advance(300);
  tabs.emitRemoved();
  const resp = await run;
  assert.equal(resp.ok, false);
  if (resp.ok) return;
  assert.equal(resp.errorCode, "content_script_not_ready");
  assert.equal(resp.message, "the ChatGPT tab was closed");
  assert.equal(content.sent.length, 0);
  assert.equal(tabs.listenerCount(), 0);
});

test("bridgeAsk.a_tab_that_is_gone_at_the_first_look_fails_without_sending", async () => {
  const tabs = fakeTabs();
  tabs.close();
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const resp = await sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);
  assert.equal(resp.ok, false);
  if (resp.ok) return;
  assert.equal(resp.message, "the ChatGPT tab was closed");
  assert.equal(content.sent.length, 0);
});

// ─── content script not ready ───────────────────────────────────────────

test("bridgeAsk.content_script_not_ready_is_retried_until_it_answers", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([failure("content_script_not_ready"), failure("content_script_not_ready"), okAnswer()]);
  const req = askReq();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);
  await advance(500); // CONTENT_READY_RETRY_MS twice
  assert.deepEqual(await run, okAnswer());
  assert.equal(content.sent.length, 3);
  assert.ok(content.sent.every((s) => s.req === req), "the same request each time");
});

test("bridgeAsk.content_script_not_ready_is_retried_at_most_20_times", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([failure("content_script_not_ready")]);
  const req = askReq();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);
  await advance(30_000);
  const resp = await run;
  assert.equal(content.sent.length, 21, "the first send and 20 retries");
  assert.deepEqual(resp, failure("content_script_not_ready"));
});

test("bridgeAsk.no_retry_when_the_deadline_leaves_no_room", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([failure("content_script_not_ready")]);
  const req = askReq({ deadlineMs: NOW + 200 }); // less than one retry delay
  const resp = await sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);
  assert.equal(content.sent.length, 1);
  assert.deepEqual(resp, failure("content_script_not_ready"));
});

test("bridgeAsk.page_reloaded_is_never_sent_again", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([failure("page_reloaded"), okAnswer()]);
  const req = askReq();
  const resp = await sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);
  await advance(2_000);
  assert.deepEqual(resp, failure("page_reloaded"));
  assert.equal(content.sent.length, 1);
});

test("bridgeAsk.any_other_error_is_returned_without_a_resend", async () => {
  for (const code of ["composer_not_found", "answer_timeout", "rate_limited", "ask_running"]) {
    const tabs = fakeTabs();
    const content = fakeContent([failure(code), okAnswer()]);
    const req = askReq();
    const resp = await sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, active(), req.deadlineMs!);
    assert.deepEqual(resp, failure(code));
    assert.equal(content.sent.length, 1, code);
  }
});

// ─── stop ───────────────────────────────────────────────────────────────

test("bridgeAsk.stop_before_the_tab_loads_sends_nothing", async () => {
  const tabs = fakeTabs({ status: "loading" });
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const ask = active();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, ask, req.deadlineMs!);

  await advance(300);
  ask.cancelled = true; // handleStopFromHost
  tabs.tab.status = "complete";
  tabs.emitUpdated({ status: "complete" });
  const resp = await run;
  assert.deepEqual(resp, {
    id: "req_1",
    ok: false,
    errorCode: "cancelled",
    message: "cancelled by stop",
  });
  assert.equal(content.sent.length, 0);
});

test("bridgeAsk.stop_already_set_sends_nothing", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([okAnswer()]);
  const req = askReq();
  const resp = await sendWhenLoaded(
    depsOf(tabs, content.sendToTab),
    TAB_ID,
    req,
    active({ cancelled: true }),
    req.deadlineMs!,
  );
  assert.equal(resp.ok, false);
  assert.equal(content.sent.length, 0);
});

test("bridgeAsk.stop_between_retries_ends_the_loop", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([failure("content_script_not_ready")]);
  const req = askReq();
  const ask = active();
  const run = sendWhenLoaded(depsOf(tabs, content.sendToTab), TAB_ID, req, ask, req.deadlineMs!);

  await advance(500);
  const before = content.sent.length;
  assert.ok(before >= 2, `${before}`);
  ask.cancelled = true;
  await advance(10_000); // long enough for every remaining retry, were there any
  const resp = await run;
  assert.equal(content.sent.length, before, "no send after the stop");
  assert.equal(resp.ok, false);
  if (!resp.ok) assert.equal(resp.errorCode, "cancelled");
});

test("bridgeAsk.stop_after_navigate_does_not_reload_the_tab", async () => {
  const tabs = fakeTabs();
  const ask = active();
  const content = fakeContent([
    () => {
      ask.cancelled = true; // the stop arrives while the content script works
      return navigate();
    },
  ]);
  const req = askReq();
  const run = askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, req, ask);
  await advance(70_000); // were the tab reloaded, it would wait out the load limit
  const resp = await run;
  assert.equal(resp.ok, false);
  if (!resp.ok) assert.equal(resp.errorCode, "cancelled");
  assert.equal(tabs.updates.length, 0);
  assert.equal(content.sent.length, 1);
});

// ─── navigate: one reload, one re-send ──────────────────────────────────

test("bridgeAsk.navigate_loads_the_tab_once_and_sends_again_marked_reloaded", async () => {
  const tabs = fakeTabs();
  loadsOnUpdate(tabs);
  const content = fakeContent([navigate(), okAnswer()]);
  const req = askReq();
  const run = askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, req, active());
  await advance(1_500);
  assert.deepEqual(await run, okAnswer());

  assert.deepEqual(tabs.updates, [{ tabId: TAB_ID, props: { url: TARGET_URL } }]);
  assert.equal(content.sent.length, 2);
  assert.equal((content.sent[0]!.req as TestAskRequest).reloaded, undefined);
  assert.deepEqual(content.sent[1]!.req, { ...req, reloaded: true });
  assert.equal(tabs.listenerCount(), 0);
});

test("bridgeAsk.the_resend_waits_for_the_new_page_not_the_old_complete", async () => {
  const tabs = fakeTabs();
  // The page that is going away still reports "complete" first.
  tabs.onUpdate = (props) => {
    tabs.emitUpdated({ status: "complete" });
    setTimeout(() => {
      tabs.tab.status = "loading";
      tabs.emitUpdated({ status: "loading" });
    }, 300);
    setTimeout(() => {
      tabs.tab.url = String(props.url);
      tabs.tab.status = "complete";
      tabs.emitUpdated({ status: "complete" });
    }, 800);
  };
  const content = fakeContent([navigate(), okAnswer()]);
  const run = askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, askReq(), active());

  await advance(500);
  assert.equal(content.sent.length, 1, "the stale complete does not release the re-send");
  await advance(500);
  assert.equal((await run).ok, true);
  assert.equal(content.sent.length, 2);
});

test("bridgeAsk.a_second_navigate_is_target_not_reached_without_another_load", async () => {
  const tabs = fakeTabs();
  loadsOnUpdate(tabs);
  const content = fakeContent([navigate()]); // every send answers "navigate"
  const run = askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, askReq(), active());
  await advance(1_500);
  const resp = await run;

  assert.equal(resp.ok, false);
  if (!resp.ok) {
    assert.equal(resp.errorCode, "target_not_reached");
    assert.equal(resp.message, "the target chat did not open after a page load");
  }
  assert.equal(tabs.updates.length, 1, "exactly one tabs.update");
  assert.equal(content.sent.length, 2, "exactly one re-send");
});

test("bridgeAsk.page_reloaded_after_the_reload_is_not_sent_again", async () => {
  const tabs = fakeTabs();
  loadsOnUpdate(tabs);
  const content = fakeContent([navigate(), failure("page_reloaded"), okAnswer()]);
  const run = askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, askReq(), active());
  await advance(3_000);
  assert.deepEqual(await run, failure("page_reloaded"));
  assert.equal(content.sent.length, 2);
  assert.equal(tabs.updates.length, 1);
});

test("bridgeAsk.content_script_not_ready_after_the_reload_is_retried_with_reloaded_set", async () => {
  const tabs = fakeTabs();
  loadsOnUpdate(tabs);
  const content = fakeContent([navigate(), failure("content_script_not_ready"), okAnswer()]);
  const run = askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, askReq(), active());
  await advance(3_000);
  assert.deepEqual(await run, okAnswer());
  assert.equal(content.sent.length, 3);
  assert.equal(tabs.updates.length, 1);
  assert.equal((content.sent[1]!.req as TestAskRequest).reloaded, true);
  assert.equal((content.sent[2]!.req as TestAskRequest).reloaded, true);
});

test("bridgeAsk.no_reload_when_too_little_time_is_left_for_the_target_chat", async () => {
  const tabs = fakeTabs();
  const content = fakeContent([navigate()]);
  // timeout 600 s → the submit budget is 15 s; 10 s are left.
  const req = askReq({ deadlineMs: NOW + 10_000 });
  const resp = await askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, req, active());
  assert.equal(resp.ok, false);
  if (!resp.ok) {
    assert.equal(resp.errorCode, "answer_timeout");
    assert.match(resp.message, /nothing was sent/);
  }
  assert.equal(tabs.updates.length, 0);
  assert.equal(content.sent.length, 1);
});

test("bridgeAsk.a_failed_tabs_update_is_content_script_not_ready_and_nothing_is_resent", async () => {
  const tabs = fakeTabs();
  tabs.onUpdate = () => {
    throw new Error("boom");
  };
  const content = fakeContent([navigate(), okAnswer()]);
  const resp = await askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, askReq(), active());
  assert.equal(resp.ok, false);
  if (!resp.ok) {
    assert.equal(resp.errorCode, "content_script_not_ready");
    assert.equal(resp.message, "could not load the target chat: boom");
  }
  assert.equal(tabs.updates.length, 1);
  assert.equal(content.sent.length, 1);
});

test("bridgeAsk.the_tab_closed_during_the_reload_ends_the_ask_without_a_resend", async () => {
  const tabs = fakeTabs();
  tabs.onUpdate = () => {
    tabs.emitUpdated({ status: "loading" });
    setTimeout(() => tabs.emitRemoved(), 300);
  };
  const content = fakeContent([navigate(), okAnswer()]);
  const run = askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, askReq(), active());
  await advance(500);
  const resp = await run;
  assert.equal(resp.ok, false);
  if (!resp.ok) assert.equal(resp.message, "the ChatGPT tab was closed");
  assert.equal(content.sent.length, 1);
});

test("bridgeAsk.a_non_navigate_answer_skips_the_reload", async () => {
  const tabs = fakeTabs();
  const status: StatusOkResponse = { id: "req_1", ok: true, kind: "status", loggedIn: true, conversationUrl: null };
  const content = fakeContent([status]);
  const resp = await askOwnTab(depsOf(tabs, content.sendToTab), TAB_ID, askReq(), active());
  assert.deepEqual(resp, status);
  assert.equal(tabs.updates.length, 0);
  assert.equal(content.sent.length, 1);
});
