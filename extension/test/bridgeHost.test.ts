// The bridge end to end on fakes: a host frame comes in through the native
// port, goes through the real routing, queue, send loop and `sendToTab`
// against a fake `chrome`, and the frame posted back is checked. Covers what
// the unit tests of the send loop (bridgeAsk.test.ts) do not: the oversize
// check on the way out, `stop` and `status` frames, and the error mapping.
// Frame shapes are the ones in protocol/tests/fixtures/.
// Run with `npm run test` (esbuild bundle + node:test, no browser).

import { after, before, beforeEach, mock, test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

// bridge.ts reads the `chrome` global when it loads; the fake goes first.
import { resetWorld, useTabs, world, type ContentReply } from "./harness/fakeChrome";
import { startBridge } from "../src/bridge";
import {
  EXTENSION_BUILD,
  MAX_HOST_FRAME_BYTES,
  askResultFrame,
  errorFrame,
  hostFrameSize,
  statusResultFrame,
  stoppedFrame,
  type ExtensionRequest,
  type ExtensionResponse,
  type TestAskRequest,
} from "../src/bridgeProtocol";
import { advance } from "./harness/clock";
import { fakeTabs, type FakeTabs } from "./harness/fakeTabs";

const NOW = 1_700_000_000_000;
const TAB_ID = 7;
const CHAT_URL = "https://chatgpt.com/c/abc-123";
const NO_LISTENER = "Could not establish connection. Receiving end does not exist.";

const FIXTURES = new URL("../../protocol/tests/fixtures/", import.meta.url);
function fixture(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(new URL(`${name}.json`, FIXTURES), "utf8"));
}
const legacy = (name: string): Record<string, unknown> => fixture(`legacy/${name}`);
const keysOf = (o: object): string[] => Object.keys(o).sort();

before(() => {
  // The keepalive interval of the bridge must not outlive the test run.
  mock.timers.enable({ apis: ["setTimeout", "setInterval", "Date"], now: NOW });
  startBridge();
});
after(() => {
  mock.timers.reset();
});

let tabs: FakeTabs;
beforeEach(() => {
  resetWorld();
  tabs = fakeTabs({ id: TAB_ID, status: "complete", url: CHAT_URL });
  useTabs(tabs);
  // The own tab the gateway's asks run in.
  world.session.ownTab = { windowId: 1, tabId: TAB_ID };
  // Every test starts with a host that reported no build (an older one), so
  // the order of the tests cannot decide whether `cancelled` goes out.
  const quiet = mock.method(console, "warn", () => {});
  try {
    world.hostMessage?.(legacy("response_pong"));
  } finally {
    quiet.mock.restore();
  }
});

function receive(msg: unknown): void {
  assert.ok(world.hostMessage, "the bridge opened its native port");
  world.hostMessage(msg);
}

// Run the clock until `done()` holds (bounded).
async function until(done: () => boolean, maxMs = 200_000): Promise<void> {
  for (let waited = 0; !done() && waited < maxMs; waited += 100) await advance(100, 100);
  assert.ok(done(), "condition was not reached");
}
const posted = (id: string): Array<Record<string, unknown>> =>
  world.posted.filter((f) => f.id === id);
const askSends = (): TestAskRequest[] =>
  world.sent.filter((s) => s.req.type === "test.ask").map((s) => s.req as TestAskRequest);

const answer = (req: ExtensionRequest, text = "the answer"): ExtensionResponse => ({
  id: req.id,
  ok: true,
  kind: "test.ask",
  text,
  conversationUrl: CHAT_URL,
});

// A reply the test releases by hand.
function gate(): { promise: Promise<ContentReply>; release: (r: ContentReply) => void } {
  let release!: (r: ContentReply) => void;
  const promise = new Promise<ContentReply>((r) => (release = r));
  return { promise, release };
}

// ─── ask ────────────────────────────────────────────────────────────────

test("bridgeHost.ask_in_the_own_tab_answers_with_an_ask_result_frame", async () => {
  world.content = async (_id, req) => answer(req);
  const ask = fixture("request_ask_conversation");
  receive(ask);
  await until(() => posted("a2").length > 0);

  assert.deepEqual(world.posted, [askResultFrame("a2", "the answer", CHAT_URL)]);
  assert.deepEqual(keysOf(world.posted[0]!), keysOf(fixture("response_ask_result")));
  // What the page was asked.
  const [sent] = askSends();
  assert.equal(sent?.text, ask.text);
  assert.equal(sent?.timeoutMs, ask.timeout_ms);
  assert.equal(sent?.deadlineMs, NOW + (ask.timeout_ms as number));
  assert.deepEqual(sent?.target, ask.target);
  assert.equal(sent?.reloaded, undefined);
});

test("bridgeHost.the_project_reaches_the_page_and_its_warning_the_host", async () => {
  const warned = fixture("response_ask_result_warning");
  world.content = async (_id, req) => ({
    id: req.id,
    ok: true,
    kind: "test.ask",
    text: warned.text as string,
    conversationUrl: warned.conversation_url as string,
    warning: warned.warning as string,
  });
  const ask = fixture("request_ask_new_chat_project");
  receive(ask);
  await until(() => posted("a5").length > 0);

  assert.deepEqual(world.posted, [warned]);
  const [sent] = askSends();
  assert.equal(sent?.project, ask.project);
  assert.deepEqual(sent?.target, ask.target);
});

test("bridgeHost.a_bad_project_is_refused_before_the_page", async () => {
  world.content = async (_id, req) => answer(req);
  for (const [i, project] of ["", " padded", "a\u0007b", "я".repeat(101), 42].entries()) {
    const id = `bad-${i}`;
    receive({ ...fixture("request_ask_new_chat_project"), id, project });
    await until(() => posted(id).length > 0);
    assert.equal(posted(id)[0]?.code, "bad_request", JSON.stringify(project));
  }
  assert.equal(askSends().length, 0);
});

test("bridgeHost.an_answer_over_the_frame_limit_becomes_an_oversize_error_for_that_ask", async () => {
  const huge = "a".repeat(MAX_HOST_FRAME_BYTES);
  world.content = async (_id, req) => answer(req, huge);
  receive(fixture("request_ask_new_chat"));
  await until(() => posted("a3").length > 0);

  const size = hostFrameSize(askResultFrame("a3", huge, CHAT_URL));
  assert.equal(world.posted.length, 1);
  const frame = world.posted[0]!;
  assert.equal(frame.type, "error");
  assert.equal(frame.id, "a3");
  assert.equal(frame.code, "oversize_frame");
  assert.ok(String(frame.message).includes(String(size)), `the message names the size ${size}`);
  assert.deepEqual(keysOf(world.posted[0]!), keysOf(fixture("response_error_oversize_frame")));
  assert.ok(!JSON.stringify(world.posted).includes("aaaa"), "no part of the answer is posted");

  // The next ask on the same tab is not affected.
  world.content = async (_id, req) => answer(req);
  receive({ ...fixture("request_ask_new_chat"), id: "a4" });
  await until(() => posted("a4").length > 0);
  assert.equal(posted("a4")[0]?.type, "ask_result");
});

test("bridgeHost.a_tab_closed_mid_ask_is_a_tab_unavailable_error_and_nothing_is_sent", async () => {
  tabs.tab.status = "loading";
  world.content = async (_id, req) => answer(req);
  receive(fixture("request_ask_new_chat"));
  await advance(500);
  assert.equal(world.posted.length, 0, "the ask waits for the page");

  tabs.emitRemoved();
  await until(() => posted("a3").length > 0);
  assert.deepEqual(world.posted, [errorFrame("a3", "tab_unavailable", "the ChatGPT tab was closed")]);
  assert.equal(askSends().length, 0);
});

test("bridgeHost.a_tab_that_never_loads_is_a_tab_unavailable_error_and_nothing_is_sent", async () => {
  tabs.tab.status = "loading";
  // `diagnose` gets no answer from the stuck page.
  world.content = (_id, req) =>
    req.type === "diagnose" ? new Promise<ContentReply>(() => {}) : Promise.resolve(answer(req));
  receive({ ...fixture("request_ask_new_chat"), timeout_ms: 600_000 });
  await until(() => posted("a3").length > 0);

  const frame = world.posted[0]!;
  assert.equal(frame.type, "error");
  assert.equal(frame.code, "tab_unavailable");
  assert.match(String(frame.message), /did not load within 60000 ms/);
  assert.equal(askSends().length, 0);
});

test("bridgeHost.no_content_script_is_retried_20_times_then_a_tab_unavailable_error", async () => {
  world.content = async () => ({ lastError: NO_LISTENER });
  receive({ ...fixture("request_ask_new_chat"), timeout_ms: 600_000 });
  await until(() => posted("a3").length > 0);

  // Each of the 21 attempts injects the page scripts once and sends again.
  assert.equal(askSends().length, 42, "the first attempt and 20 retries, each sent twice");
  assert.equal(world.injected.filter((i) => i.file === "content.js").length, 21);
  const frame = world.posted[0]!;
  assert.equal(frame.code, "tab_unavailable");
  assert.match(String(frame.message), /Receiving end does not exist/);
});

// ─── a tab open from before the extension was installed or reloaded ─────

const CLI_ASK = { type: "ask", id: "c1", text: "hello", timeout_ms: 60_000 };

// A CLI ask (no target) goes to the user's active ChatGPT tab: the fake tab,
// with no own tab recorded.
function userTab(url = CHAT_URL, title = "Trip plans"): void {
  world.session = {};
  tabs.tab.url = url;
  tabs.tab.title = title;
  (chrome.tabs as unknown as Record<string, unknown>).query = async () => [{ ...tabs.tab, active: true }];
}

test("bridgeHost.a_tab_without_content_script_gets_the_page_scripts_and_the_same_ask_once_more", async () => {
  userTab();
  world.inject = async (i) => {
    if (i.file === "content.js") world.content = async (_id, req) => answer(req);
  };
  world.content = async () => ({ lastError: NO_LISTENER });
  receive(CLI_ASK);
  await until(() => posted("c1").length > 0);

  assert.deepEqual(posted("c1"), [askResultFrame("c1", "the answer", CHAT_URL)]);
  // The shim into the page's own world first, then the content script.
  assert.deepEqual(world.injected, [
    { tabId: TAB_ID, file: "pageShim.js", world: "MAIN" },
    { tabId: TAB_ID, file: "content.js", world: "ISOLATED" },
  ]);
  const [first, again, ...more] = askSends();
  assert.deepEqual(again, first, "the very same request, once more");
  assert.equal(more.length, 0);
});

test("bridgeHost.a_tab_still_without_content_script_after_the_injection_is_named_in_the_error", async () => {
  userTab(`${CHAT_URL}?model=gpt-5&q=secret#frag`, "Trip plans");
  world.content = async () => ({ lastError: NO_LISTENER });
  receive(CLI_ASK);
  await until(() => posted("c1").length > 0);

  assert.equal(askSends().length, 2, "one send after the one injection");
  assert.equal(world.injected.filter((i) => i.file === "content.js").length, 1);
  const frame = posted("c1")[0]!;
  assert.equal(frame.code, "tab_unavailable");
  const message = String(frame.message);
  assert.match(message, /Receiving end does not exist/);
  assert.ok(message.includes(`"Trip plans" at ${CHAT_URL}`), message);
  assert.ok(!message.includes("model=") && !message.includes("secret") && !message.includes("frag"), message);
});

test("bridgeHost.a_tab_that_refuses_the_injection_is_not_asked_again", async () => {
  userTab();
  world.inject = async (i) => {
    if (i.file === "content.js") throw new Error("Cannot access contents of the page.");
  };
  world.content = async () => ({ lastError: NO_LISTENER });
  receive(CLI_ASK);
  await until(() => posted("c1").length > 0);

  assert.equal(askSends().length, 1);
  const frame = posted("c1")[0]!;
  assert.equal(frame.code, "tab_unavailable");
  assert.match(String(frame.message), /Cannot access contents of the page/);
  assert.match(String(frame.message), /"Trip plans"/);
});

test("bridgeHost.a_stop_while_the_page_scripts_go_in_ends_the_ask_at_once_and_nothing_is_sent", async () => {
  // A host that knows `cancelled` (its build is not this one: a warning).
  const quiet = mock.method(console, "warn", () => {});
  try {
    receive(fixture("response_pong"));
  } finally {
    quiet.mock.restore();
  }
  // The content script goes in after 1 s, and would then answer.
  world.inject = (i) =>
    i.file !== "content.js"
      ? Promise.resolve()
      : new Promise<void>((resolve) =>
          setTimeout(() => {
            world.content = async (_id, req) => answer(req);
            resolve();
          }, 1_000),
        );
  world.content = async () => ({ lastError: NO_LISTENER });
  receive(fixture("request_ask_conversation"));
  await until(() => world.injected.some((i) => i.file === "content.js"));

  const stopAt = Date.now();
  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0 && posted("a2").length > 0);
  assert.ok(Date.now() - stopAt < 1_000, "the ask ended before the injection did");
  // Between page loads: the prompt has not reached the page, and it never will.
  assert.deepEqual(posted("s1"), [stoppedFrame("s1", false)]);
  assert.deepEqual(posted("a2"), [errorFrame("a2", "cancelled", "cancelled by stop")]);

  await advance(2_000);
  assert.equal(askSends().length, 1, "only the first send, which nobody received");
});

test("bridgeHost.a_tab_that_left_chatgpt_gets_nothing_injected_and_is_named_without_its_query", async () => {
  userTab();
  // Chosen on chatgpt.com, then sent to sign-in before the message arrived.
  world.content = async () => {
    tabs.tab.url = "https://auth.openai.com/log-in?state=s3cret#frag";
    return { lastError: NO_LISTENER };
  };
  receive(CLI_ASK);
  await until(() => posted("c1").length > 0);

  assert.deepEqual(world.injected, []);
  assert.equal(askSends().length, 1);
  const message = String(posted("c1")[0]!.message);
  assert.equal(posted("c1")[0]!.code, "tab_unavailable");
  assert.ok(message.includes("left chatgpt.com") && message.includes("https://auth.openai.com/log-in"), message);
  assert.ok(!message.includes("s3cret") && !message.includes("frag"), message);
});

test("bridgeHost.an_address_in_chromes_injection_error_loses_its_query", async () => {
  userTab();
  world.inject = async (i) => {
    if (i.file === "content.js") {
      throw new Error(
        'Cannot access contents of url "https://auth.openai.com/log-in?state=s3cret#frag". ' +
          "Extension manifest must request permission to access this host.",
      );
    }
  };
  world.content = async () => ({ lastError: NO_LISTENER });
  receive(CLI_ASK);
  await until(() => posted("c1").length > 0);

  const message = String(posted("c1")[0]!.message);
  assert.ok(message.includes('Cannot access contents of url "https://auth.openai.com/log-in"'), message);
  assert.ok(!message.includes("s3cret") && !message.includes("frag"), message);
});

test("bridgeHost.a_page_that_never_takes_the_injected_script_is_given_up_on_after_5_s", async () => {
  userTab();
  // A page stuck loading never reaches document_idle, where the script goes in.
  world.inject = (i) => (i.file === "content.js" ? new Promise<void>(() => {}) : Promise.resolve());
  world.content = async () => ({ lastError: NO_LISTENER });
  const sentAt = Date.now();
  receive(CLI_ASK);
  await until(() => posted("c1").length > 0);

  assert.equal(Date.now() - sentAt, 5_000);
  assert.equal(askSends().length, 1);
  const frame = posted("c1")[0]!;
  assert.equal(frame.code, "tab_unavailable");
  assert.match(String(frame.message), /could not be injected: .*within 5000 ms/);
});

test("bridgeHost.a_page_that_goes_away_mid_turn_is_a_dom_failure_and_is_not_asked_again", async () => {
  world.content = async () => ({ lastError: "The message port closed before a response was received." });
  receive({ ...fixture("request_ask_new_chat"), timeout_ms: 600_000 });
  await until(() => posted("a3").length > 0);
  await advance(2_000);

  assert.equal(askSends().length, 1);
  assert.deepEqual(world.injected, [], "the page took the request: nothing is injected");
  assert.equal(world.posted.length, 1);
  assert.equal(world.posted[0]?.code, "dom_failure");
  assert.match(String(world.posted[0]?.message), /the page reloaded during the turn/);
});

test("bridgeHost.an_empty_reply_from_the_tab_is_a_dom_failure_and_is_not_asked_again", async () => {
  world.content = async () => undefined;
  receive({ ...fixture("request_ask_new_chat"), timeout_ms: 600_000 });
  await until(() => posted("a3").length > 0);
  assert.equal(askSends().length, 1);
  assert.equal(world.posted[0]?.code, "dom_failure");
});

test("bridgeHost.a_signed_out_page_is_a_tab_unavailable_error_with_the_pages_message", async () => {
  const message = "ChatGPT в окне cgpt-bridge не выполнил вход — войдите в этом окне";
  world.content = async (_id, req) => ({ id: req.id, ok: false, errorCode: "not_logged_in", message });
  receive(fixture("request_ask_new_chat"));
  await until(() => posted("a3").length > 0);

  assert.deepEqual(world.posted, [errorFrame("a3", "tab_unavailable", message)]);
  assert.equal(askSends().length, 1, "not asked again");
});

test("bridgeHost.navigate_reloads_the_own_tab_once_and_asks_again_with_reloaded", async () => {
  const target = "https://chatgpt.com/c/abc-123";
  tabs.tab.url = "https://chatgpt.com/";
  tabs.onUpdate = (props) => {
    tabs.tab.status = "loading";
    tabs.emitUpdated({ status: "loading" });
    setTimeout(() => {
      tabs.tab.url = String(props.url);
      tabs.tab.status = "complete";
      tabs.emitUpdated({ status: "complete" });
    }, 500);
  };
  let n = 0;
  world.content = async (_id, req) =>
    ++n === 1 ? { id: req.id, ok: true, kind: "navigate", url: target } : answer(req);
  receive(fixture("request_ask_conversation"));
  await until(() => posted("a2").length > 0);

  assert.deepEqual(world.posted, [askResultFrame("a2", "the answer", CHAT_URL)]);
  assert.deepEqual(tabs.updates, [{ tabId: TAB_ID, props: { url: target } }]);
  assert.deepEqual(
    askSends().map((r) => r.reloaded),
    [undefined, true],
  );
});

// ─── one id, one outcome line ───────────────────────────────────────────

// What the service worker console got from `console.info` while `run` ran.
async function infoLines(run: () => Promise<void>): Promise<string[]> {
  const info = mock.method(console, "info", () => {});
  try {
    await run();
    return info.mock.calls.map((c) => c.arguments.map(String).join(" "));
  } finally {
    info.mock.restore();
  }
}
const outcomeLines = (lines: string[]): string[] => lines.filter((l) => l.includes("outcome="));

test("bridgeHost.the_request_to_the_page_starts_with_the_hosts_id", async () => {
  world.content = async (_id, req) => answer(req);
  receive({ ...fixture("request_ask_conversation"), id: "gw_ask_1f_18c_2" });
  await until(() => posted("gw_ask_1f_18c_2").length > 0);

  const [sent] = askSends();
  assert.ok(sent?.id.startsWith("gw_ask_1f_18c_2"), `page request id: ${sent?.id}`);
  assert.match(sent!.id, /^[A-Za-z0-9_.-]{1,80}$/, "stays in the safe alphabet");
  // The wire frame back to the host keeps the host's own id.
  assert.equal(posted("gw_ask_1f_18c_2")[0]?.type, "ask_result");
});

test("bridgeHost.an_answered_ask_leaves_one_outcome_line_with_id_tab_code_and_duration", async () => {
  world.content = async (_id, req) => answer(req, "SECRET-ANSWER");
  const ask = { ...fixture("request_ask_conversation"), id: "gw_ask_a_b_c", text: "SECRET-PROMPT" };
  const lines = await infoLines(async () => {
    receive(ask);
    await until(() => posted("gw_ask_a_b_c").length > 0);
  });

  const out = outcomeLines(lines);
  assert.equal(out.length, 1, `one line per ask: ${JSON.stringify(lines)}`);
  assert.match(out[0]!, /^\[cgpt-bridge\] ask gw_ask_a_b_c tab=7 outcome=ok \d+ ms$/);
  assert.ok(!lines.join("\n").includes("SECRET"), "no prompt or answer text in the log");
});

test("bridgeHost.a_failed_ask_leaves_one_outcome_line_with_the_wire_code", async () => {
  world.content = async (_id, req) => ({
    id: req.id,
    ok: false,
    errorCode: "answer_timeout",
    message: "SECRET-DETAIL",
  });
  const lines = await infoLines(async () => {
    receive({ ...fixture("request_ask_new_chat"), id: "gw_ask_f" });
    await until(() => posted("gw_ask_f").length > 0);
  });

  const out = outcomeLines(lines);
  assert.equal(out.length, 1);
  assert.match(out[0]!, /^\[cgpt-bridge\] ask gw_ask_f tab=7 outcome=timeout \d+ ms$/);
  assert.ok(!lines.join("\n").includes("SECRET"));
});

test("bridgeHost.an_ask_refused_before_a_tab_is_chosen_still_leaves_its_outcome_line", async () => {
  const lines = await infoLines(async () => {
    receive({ ...fixture("request_ask_new_chat"), id: "gw_ask_b", target: { kind: "nowhere" } });
    await until(() => posted("gw_ask_b").length > 0);
  });

  const out = outcomeLines(lines);
  assert.equal(out.length, 1);
  assert.match(out[0]!, /^\[cgpt-bridge\] ask gw_ask_b tab=- outcome=bad_request \d+ ms$/);
  assert.equal(askSends().length, 0);
});

test("bridgeHost.the_outcome_line_counts_the_time_the_ask_took", async () => {
  const held = gate();
  world.content = (_id, req) => (req.type === "test.ask" ? held.promise : Promise.resolve(undefined));
  const lines = await infoLines(async () => {
    receive({ ...fixture("request_ask_new_chat"), id: "gw_ask_t" });
    await until(() => askSends().length === 1);
    await advance(3_000);
    held.release(answer(askSends()[0]!));
    await until(() => posted("gw_ask_t").length > 0);
  });

  const ms = Number(/outcome=ok (\d+) ms/.exec(outcomeLines(lines)[0] ?? "")?.[1]);
  assert.ok(ms >= 3_000 && ms < 60_000, `duration ${ms} ms`);
});

test("bridgeHost.an_oversize_answer_leaves_an_oversize_frame_outcome_line", async () => {
  const huge = "a".repeat(MAX_HOST_FRAME_BYTES);
  world.content = async (_id, req) => answer(req, huge);
  const lines = await infoLines(async () => {
    receive({ ...fixture("request_ask_new_chat"), id: "gw_ask_o" });
    await until(() => posted("gw_ask_o").length > 0);
  });

  const out = outcomeLines(lines);
  assert.equal(out.length, 1, JSON.stringify(lines));
  assert.match(out[0]!, /^\[cgpt-bridge\] ask gw_ask_o tab=7 outcome=oversize_frame \d+ ms$/);
});

test("bridgeHost.an_answer_the_closed_port_could_not_carry_is_logged_undelivered_not_ok", async () => {
  const held = gate();
  world.content = (_id, req) => (req.type === "test.ask" ? held.promise : Promise.resolve(undefined));
  const lines = await infoLines(async () => {
    receive({ ...fixture("request_ask_new_chat"), id: "gw_ask_u" });
    await until(() => askSends().length === 1);
    world.disconnect(); // the host went away while the page was still answering
    held.release(answer(askSends()[0]!));
    await advance(1_000);
  });
  // Let the bridge reconnect (backoff 5 s) so the next tests have a port.
  await advance(6_000);

  assert.equal(posted("gw_ask_u").length, 0, "nothing could be posted");
  const out = outcomeLines(lines);
  assert.equal(out.length, 1, JSON.stringify(lines));
  assert.match(out[0]!, /^\[cgpt-bridge\] ask gw_ask_u tab=7 outcome=undelivered \d+ ms$/);
});

test("bridgeHost.a_stop_leaves_one_outcome_line_for_the_running_and_for_the_queued_ask", async () => {
  const held = gate();
  let asks = 0;
  world.content = (_id, req) => {
    if (req.type === "test.ask") return ++asks === 1 ? held.promise : Promise.resolve(answer(req));
    if (req.type === "stop") return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: true });
    return Promise.resolve({ lastError: "unexpected" });
  };
  const lines = await infoLines(async () => {
    receive({ ...fixture("request_ask_conversation"), id: "gw_ask_r" });
    await until(() => askSends().length === 1);
    receive({ ...fixture("request_ask_conversation"), id: "gw_ask_q" });
    await advance(500);
    receive(fixture("request_stop"));
    await until(() => posted("s1").length > 0);
    held.release({ id: askSends()[0]!.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" });
    await until(() => posted("gw_ask_r").length > 0 && posted("gw_ask_q").length > 0);
    await advance(2_000);
  });

  const out = outcomeLines(lines);
  assert.equal(out.length, 2, JSON.stringify(lines));
  // The queued ask was never sent to the page but belongs to the same tab.
  // `cancelled` even when the host is too old for the code (see below).
  assert.ok(out.some((l) => /ask gw_ask_r tab=7 outcome=cancelled \d+ ms$/.test(l)), out.join("\n"));
  assert.ok(out.some((l) => /ask gw_ask_q tab=7 outcome=cancelled \d+ ms$/.test(l)), out.join("\n"));
  assert.equal(askSends().length, 1);
});

// ─── stop ───────────────────────────────────────────────────────────────

test("bridgeHost.stop_during_an_ask_names_the_ask_and_the_ask_ends_cancelled", async () => {
  const held = gate();
  world.content = (_id, req) => {
    if (req.type === "test.ask") return held.promise;
    if (req.type === "stop") return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: true });
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation"));
  await until(() => askSends().length === 1);

  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);

  const stopReq = world.sent.find((s) => s.req.type === "stop")?.req;
  assert.deepEqual(stopReq && { type: stopReq.type, askId: (stopReq as { askId?: string }).askId }, {
    type: "stop",
    askId: askSends()[0]?.id,
  });
  assert.deepEqual(posted("s1"), [stoppedFrame("s1", true)]);
  assert.equal(posted("a2").length, 0, "the ask has not been answered yet");

  held.release({ id: askSends()[0]!.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" });
  await until(() => posted("a2").length > 0);
  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
});

test("bridgeHost.stop_cancels_an_ask_queued_behind_a_running_one_and_it_is_never_sent", async () => {
  const held = gate();
  let asks = 0;
  world.content = (_id, req) => {
    if (req.type === "test.ask") return ++asks === 1 ? held.promise : Promise.resolve(answer(req));
    if (req.type === "stop") return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: true });
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation")); // a2 runs and waits for its answer
  await until(() => askSends().length === 1);
  receive({ ...fixture("request_ask_conversation"), id: "a5" }); // a5 waits behind a2
  await advance(500);
  assert.equal(askSends().length, 1, "a5 is queued, not sent");

  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);
  assert.deepEqual(posted("s1"), [stoppedFrame("s1", true)]);

  held.release({ id: askSends()[0]!.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" });
  await until(() => posted("a2").length > 0 && posted("a5").length > 0);
  await advance(2_000);

  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
  assert.deepEqual(posted("a5"), [errorFrame("a5", "internal", "cancelled by stop")]);
  assert.equal(askSends().length, 1, "the queued ask never reaches the tab");
});

test("bridgeHost.stop_while_the_tab_is_loading_means_the_prompt_is_never_sent", async () => {
  tabs.tab.status = "loading";
  world.content = async (_id, req) =>
    req.type === "stop" ? { lastError: NO_LISTENER } : answer(req);
  receive(fixture("request_ask_conversation"));
  await advance(500);

  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);
  // Between page loads: the prompt has not reached the page.
  assert.deepEqual(posted("s1"), [stoppedFrame("s1", false)]);
  // A content script injected now would know of no ask to stop.
  assert.deepEqual(world.injected, []);

  tabs.tab.status = "complete";
  tabs.emitUpdated({ status: "complete" });
  await until(() => posted("a2").length > 0);
  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
  assert.equal(askSends().length, 0, "not a single send of the prompt");
});

test("bridgeHost.stop_with_no_ask_in_flight_presses_stop_in_the_own_tab", async () => {
  world.content = async (_id, req) =>
    req.type === "stop" ? { id: req.id, ok: true, kind: "stop", wasGenerating: true } : { lastError: "x" };
  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);

  assert.deepEqual(posted("s1"), [stoppedFrame("s1", true)]);
  assert.deepEqual(
    world.sent.map((s) => [s.tabId, s.req.type, (s.req as { askId?: string }).askId]),
    [[TAB_ID, "stop", undefined]],
  );
});

test("bridgeHost.stop_with_no_own_tab_is_answered_without_touching_any_tab", async () => {
  world.session = {};
  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);
  assert.deepEqual(posted("s1"), [stoppedFrame("s1", false)]);
  assert.equal(world.sent.length, 0);
});

// ─── stop_ask: one named ask (Ctrl+C in `cgpt ask`) ─────────────────────

const USER_TAB_ID = 9;

// The user's active ChatGPT tab, where a CLI ask (no target) goes; the own
// tab stays TAB_ID.
function withUserTab(): void {
  (chrome.tabs as unknown as { query: () => Promise<unknown[]> }).query = async () => [
    { id: USER_TAB_ID, active: true, url: "https://chatgpt.com/c/user-chat" },
  ];
}

const stopsSent = (): Array<[number, string | undefined]> =>
  world.sent
    .filter((s) => s.req.type === "stop")
    .map((s) => [s.tabId, (s.req as { askId?: string }).askId]);

test("bridgeHost.stop_ask_stops_only_that_cli_ask_in_the_user_tab", async () => {
  withUserTab();
  const cli = gate();
  const gw = gate();
  world.content = (tabId, req) => {
    if (req.type === "test.ask") return tabId === USER_TAB_ID ? cli.promise : gw.promise;
    if (req.type === "stop") return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: true });
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation")); // a2: the gateway's, own tab
  receive(fixture("request_ask")); // a1: the CLI's, user tab
  await until(() => askSends().length === 2);
  const cliAsk = world.sent.find((s) => s.tabId === USER_TAB_ID && s.req.type === "test.ask")!;

  const stop = fixture("request_stop_ask");
  assert.deepEqual([stop.type, stop.ask_id], ["stop_ask", "a1"]);
  receive(stop);
  await until(() => posted("s2").length > 0);

  // Stop is pressed for the CLI ask, in its tab, and nowhere else.
  assert.deepEqual(stopsSent(), [[USER_TAB_ID, cliAsk.req.id]]);
  assert.deepEqual(posted("s2"), [stoppedFrame("s2", true)]);

  cli.release({ id: cliAsk.req.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" });
  await until(() => posted("a1").length > 0);
  assert.deepEqual(posted("a1"), [errorFrame("a1", "internal", "cancelled by stop")]);

  // The gateway's ask was not cancelled: its answer still goes out.
  const gwAsk = world.sent.find((s) => s.tabId === TAB_ID && s.req.type === "test.ask")!;
  gw.release(answer(gwAsk.req));
  await until(() => posted("a2").length > 0);
  assert.deepEqual(posted("a2"), [askResultFrame("a2", "the answer", CHAT_URL)]);
});

test("bridgeHost.stop_ask_refused_in_the_page_tells_the_host_the_tab_shows_another_chat", async () => {
  withUserTab();
  const cli = gate();
  world.content = (_tabId, req) => {
    if (req.type === "test.ask") return cli.promise;
    if (req.type === "stop") {
      return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: false, otherChat: true });
    }
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask")); // a1: the CLI's, user tab
  await until(() => askSends().length === 1);
  receive(fixture("request_stop_ask"));
  await until(() => posted("s2").length > 0);
  assert.deepEqual(posted("s2"), [fixture("response_stopped_other_chat")]);
  cli.release({ id: askSends()[0]!.id, ok: false, errorCode: "target_mismatch", message: "left" });
  await until(() => posted("a1").length > 0);
});

test("bridgeHost.stop_ask_with_an_unknown_id_touches_nothing_and_leaves_the_gateway_ask", async () => {
  const gw = gate();
  world.content = (_tabId, req) => {
    if (req.type === "test.ask") return gw.promise;
    if (req.type === "stop") return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: true });
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation")); // a2: the gateway's, own tab
  await until(() => askSends().length === 1);

  receive({ ...fixture("request_stop_ask"), ask_id: "gone" });
  await until(() => posted("s2").length > 0);
  assert.deepEqual(posted("s2"), [stoppedFrame("s2", false)]);
  assert.deepEqual(stopsSent(), [], "no Stop pressed in any tab");

  gw.release(answer(askSends()[0]!));
  await until(() => posted("a2").length > 0);
  assert.deepEqual(posted("a2"), [askResultFrame("a2", "the answer", CHAT_URL)]);
});

test("bridgeHost.stop_ask_with_no_ask_in_flight_never_falls_back_to_the_own_tab", async () => {
  world.content = async (_id, req) =>
    req.type === "stop" ? { id: req.id, ok: true, kind: "stop", wasGenerating: true } : { lastError: "x" };
  receive(fixture("request_stop_ask"));
  await until(() => posted("s2").length > 0);
  assert.deepEqual(posted("s2"), [stoppedFrame("s2", false)]);
  assert.deepEqual(stopsSent(), []);
});

// ─── a stop while the tab is still being chosen ────────────────────────

// Holds every `chrome.tabs.<name>` call until `release()`: the tab choice
// (`tabForAsk`) can take seconds, say while an own window is adopted.
function holdTabs(name: "get" | "query"): () => void {
  const api = chrome.tabs as unknown as Record<string, (...args: unknown[]) => Promise<unknown>>;
  const real = api[name]!;
  let release!: () => void;
  const held = new Promise<void>((r) => (release = r));
  api[name] = async (...args: unknown[]) => {
    await held;
    return real(...args);
  };
  return release;
}

test("bridgeHost.stop_ask_while_the_cli_asks_tab_is_chosen_ends_it_cancelled_and_nothing_is_sent", async () => {
  withUserTab();
  const release = holdTabs("query");
  world.content = async (_id, req) =>
    req.type === "stop" ? { id: req.id, ok: true, kind: "stop", wasGenerating: true } : answer(req);
  receive(fixture("request_ask")); // a1: the CLI's, still choosing the user tab
  await advance(500);

  receive(fixture("request_stop_ask"));
  await until(() => posted("s2").length > 0);
  // Known to the extension and stopped before anything reached a tab.
  assert.deepEqual(posted("s2"), [fixture("response_stopped_not_sent")]);
  assert.deepEqual(posted("s2"), [stoppedFrame("s2", false, false, true)]);

  release();
  await until(() => posted("a1").length > 0);
  assert.deepEqual(posted("a1"), [errorFrame("a1", "internal", "cancelled by stop")]);
  assert.equal(askSends().length, 0, "the prompt never reaches the tab");
  assert.deepEqual(stopsSent(), [], "no Stop pressed in any tab");
});

test("bridgeHost.stop_while_the_own_tab_is_chosen_ends_the_gateway_ask_cancelled_and_nothing_is_sent", async () => {
  const release = holdTabs("get");
  world.content = async (_id, req) =>
    req.type === "stop" ? { id: req.id, ok: true, kind: "stop", wasGenerating: false } : answer(req);
  receive(fixture("request_ask_conversation")); // a2: the gateway's, own tab not found yet
  await advance(500);

  receive(fixture("request_stop"));
  await advance(500);
  release();
  await until(() => posted("s1").length > 0 && posted("a2").length > 0);

  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
  assert.equal(askSends().length, 0, "the prompt never reaches the tab");
  // As with no ask in a tab: Stop only if an earlier answer still generates.
  assert.deepEqual(posted("s1"), [stoppedFrame("s1", false)]);
  assert.deepEqual(stopsSent(), [[TAB_ID, undefined]]);
});

test("bridgeHost.stop_ask_for_a_cli_ask_queued_behind_another_says_nothing_was_sent", async () => {
  withUserTab();
  const first = gate();
  let asks = 0;
  world.content = (_tabId, req) => {
    if (req.type === "test.ask") return ++asks === 1 ? first.promise : Promise.resolve(answer(req));
    if (req.type === "stop") return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: true });
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive({ ...fixture("request_ask"), id: "a0" }); // runs in the user tab
  await until(() => askSends().length === 1);
  receive(fixture("request_ask")); // a1 waits behind a0 in the same tab
  await advance(500);

  receive(fixture("request_stop_ask"));
  await until(() => posted("s2").length > 0);
  assert.deepEqual(posted("s2"), [stoppedFrame("s2", false, false, true)]);
  assert.deepEqual(stopsSent(), [], "the running ask is left alone");

  first.release(answer(askSends()[0]!));
  await until(() => posted("a0").length > 0 && posted("a1").length > 0);
  assert.deepEqual(posted("a0"), [askResultFrame("a0", "the answer", CHAT_URL)]);
  assert.deepEqual(posted("a1"), [errorFrame("a1", "internal", "cancelled by stop")]);
  assert.equal(askSends().length, 1, "the queued ask never reaches the tab");
});

test("bridgeHost.stop_ask_before_the_page_sent_the_prompt_tells_the_host_nothing_was_sent", async () => {
  withUserTab();
  const cli = gate();
  world.content = (_tabId, req) => {
    if (req.type === "test.ask") return cli.promise;
    if (req.type === "stop") {
      return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: false, notSent: true });
    }
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask"));
  await until(() => askSends().length === 1);
  receive(fixture("request_stop_ask"));
  await until(() => posted("s2").length > 0);
  assert.deepEqual(posted("s2"), [fixture("response_stopped_not_sent")]);
  cli.release({ id: askSends()[0]!.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" });
  await until(() => posted("a1").length > 0);
});

test("bridgeHost.a_gateway_stop_never_carries_not_sent", async () => {
  const held = gate();
  world.content = (_tabId, req) => {
    if (req.type === "test.ask") return held.promise;
    if (req.type === "stop") {
      return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: false, notSent: true });
    }
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation"));
  await until(() => askSends().length === 1);
  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);
  assert.deepEqual(posted("s1"), [stoppedFrame("s1", false)]);
  held.release({ id: askSends()[0]!.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" });
  await until(() => posted("a2").length > 0);
});

// ─── status ─────────────────────────────────────────────────────────────

test("bridgeHost.status_without_an_own_tab_says_the_tab_is_not_open", async () => {
  world.session = {};
  receive(fixture("request_status"));
  await until(() => posted("t1").length > 0);
  assert.deepEqual(world.posted, [
    statusResultFrame("t1", { tabOpen: false, loggedIn: false, conversationUrl: null }),
  ]);
  assert.deepEqual(keysOf(world.posted[0]!), keysOf(fixture("response_status_result")));
  assert.equal(world.sent.length, 0);
});

test("bridgeHost.status_takes_login_and_chat_from_the_page", async () => {
  world.content = async (_id, req) => ({
    id: req.id,
    ok: true,
    kind: "status",
    loggedIn: true,
    conversationUrl: "https://chatgpt.com/c/from-page",
  });
  receive(fixture("request_status"));
  await until(() => posted("t1").length > 0);
  assert.deepEqual(world.posted, [
    statusResultFrame("t1", {
      tabOpen: true,
      loggedIn: true,
      conversationUrl: "https://chatgpt.com/c/from-page",
    }),
  ]);
});

test("bridgeHost.status_of_a_page_that_does_not_answer_is_not_logged_in_with_the_chat_from_the_tab_url", async () => {
  world.content = async () => ({ lastError: NO_LISTENER });
  receive(fixture("request_status"));
  await until(() => posted("t1").length > 0);
  // The very frame of the fixture: open, not logged in, the chat of the URL
  // (and this extension's build).
  assert.deepEqual(world.posted, [{ ...fixture("response_status_result"), build: EXTENSION_BUILD }]);
});

test("bridgeHost.status_of_a_tab_that_left_chatgpt_says_the_tab_is_not_open", async () => {
  tabs.tab.url = "https://example.com/";
  receive(fixture("request_status"));
  await until(() => posted("t1").length > 0);
  assert.equal(world.posted[0]?.tab_open, false);
  assert.equal(world.sent.length, 0);
});

// ─── build ──────────────────────────────────────────────────────────────

// An ask the page answers only once `stop` came: the page says "cancelled".
async function stoppedAsk(): Promise<void> {
  const held = gate();
  world.content = (_id, req) => {
    if (req.type === "test.ask") return held.promise;
    if (req.type === "stop") return Promise.resolve({ id: req.id, ok: true, kind: "stop", wasGenerating: true });
    return Promise.resolve({ lastError: "unexpected" });
  };
  receive(fixture("request_ask_conversation"));
  await until(() => askSends().length === 1);
  receive(fixture("request_stop"));
  await until(() => posted("s1").length > 0);
  held.release({ id: askSends()[0]!.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" });
  await until(() => posted("a2").length > 0);
}

test("bridgeHost.status_result_carries_the_extensions_build", async () => {
  world.session = {};
  receive(fixture("request_status"));
  await until(() => posted("t1").length > 0);
  assert.equal(world.posted[0]?.build, EXTENSION_BUILD);
});

test("bridgeHost.a_host_that_reports_a_build_gets_a_stopped_ask_as_cancelled", async () => {
  receive(fixture("response_pong"));
  try {
    const lines = await infoLines(stoppedAsk);
    assert.deepEqual(posted("a2"), [errorFrame("a2", "cancelled", "cancelled by stop")]);
    assert.match(outcomeLines(lines)[0] ?? "", /ask a2 tab=7 outcome=cancelled \d+ ms$/);
  } finally {
    receive(legacy("response_pong"));
  }
});

test("bridgeHost.a_new_connection_forgets_the_hosts_build_until_its_pong", async () => {
  receive(fixture("response_pong"));
  // The host went away; Chrome starts a host again, which has not answered
  // the ping yet and may be another binary.
  world.disconnect();
  await advance(6_000);
  await stoppedAsk();
  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
});

test("bridgeHost.a_pong_whose_build_is_not_a_build_id_does_not_open_the_cancelled_gate", async () => {
  receive({ ...fixture("response_pong"), build: "x y\u001b" });
  await stoppedAsk();
  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
});

test("bridgeHost.an_older_host_gets_a_stopped_ask_as_internal_with_the_exact_message", async () => {
  // A host without `build` predates `cancelled` and would fail the frame.
  receive(legacy("response_pong"));
  const lines = await infoLines(stoppedAsk);
  assert.deepEqual(posted("a2"), [errorFrame("a2", "internal", "cancelled by stop")]);
  // The outcome line still says what happened.
  assert.match(outcomeLines(lines)[0] ?? "", /ask a2 tab=7 outcome=cancelled \d+ ms$/);
});

// An ask the page refuses because the tab is busy (`errorCode` is the page's).
async function busyAsk(id: string, errorCode: string, message: string): Promise<string[]> {
  world.content = async (_id, req) => ({ id: req.id, ok: false, errorCode, message });
  return infoLines(async () => {
    receive({ ...fixture("request_ask_new_chat"), id });
    await until(() => posted(id).length > 0);
  });
}

test("bridgeHost.a_host_that_reports_a_build_gets_a_busy_tab_as_tab_busy", async () => {
  receive(fixture("response_pong"));
  try {
    for (const [i, code] of ["previous_answer_running", "ask_running"].entries()) {
      const id = `gw_ask_busy${i}`;
      const lines = await busyAsk(id, code, "busy here");
      assert.deepEqual(posted(id), [errorFrame(id, "tab_busy", "busy here")], code);
      assert.match(outcomeLines(lines)[0] ?? "", new RegExp(`ask ${id} tab=7 outcome=tab_busy \\d+ ms$`));
    }
  } finally {
    receive(legacy("response_pong"));
  }
});

test("bridgeHost.an_older_host_gets_a_busy_tab_as_dom_failure_with_the_same_message", async () => {
  // A host without `build` predates `tab_busy` and would fail the frame.
  receive(legacy("response_pong"));
  const lines = await busyAsk("gw_ask_old", "previous_answer_running", "busy here");
  assert.deepEqual(posted("gw_ask_old"), [errorFrame("gw_ask_old", "dom_failure", "busy here")]);
  // The outcome line still says what happened.
  assert.match(outcomeLines(lines)[0] ?? "", /ask gw_ask_old tab=7 outcome=tab_busy \d+ ms$/);
});

test("bridgeHost.the_hosts_build_is_logged_when_it_changes_with_a_warning_when_it_differs", async () => {
  const warn = mock.method(console, "warn", () => {});
  try {
    const lines = await infoLines(async () => {
      receive({ ...fixture("response_pong"), build: EXTENSION_BUILD });
      receive({ ...fixture("response_pong"), build: EXTENSION_BUILD }); // keepalive: no new line
      receive({ ...fixture("response_pong"), build: "other" });
      receive(legacy("response_pong"));
    });
    const hostLines = (all: string[]): string[] => all.filter((l) => l.includes("native host build"));
    assert.deepEqual(hostLines(lines), [`[cgpt-bridge] native host build ${EXTENSION_BUILD}`]);
    assert.deepEqual(hostLines(warn.mock.calls.map((c) => c.arguments.map(String).join(" "))), [
      `[cgpt-bridge] native host build other differs from the extension build ${EXTENSION_BUILD}`,
      `[cgpt-bridge] native host build (none) differs from the extension build ${EXTENSION_BUILD}`,
    ]);
  } finally {
    warn.mock.restore();
  }
});
