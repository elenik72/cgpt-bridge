// Pure-logic tests for how the background runs bridge asks: one at a time
// per tab, within the caller's deadline, cancellable by `stop`.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  InFlight,
  TabQueue,
  isReceivingEndMissing,
  parseHostTarget,
  stopActionFor,
  submitBudgetMs,
  takeStop,
  type PageAskState,
} from "../src/askControl";
import { targetMatchesPath, textAtDeadline } from "../src/chatgptAdapter";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

test("askControl.tab_queue_runs_asks_one_at_a_time_per_tab", async () => {
  const queue = new TabQueue();
  const log: string[] = [];
  const job = (name: string, ms: number) => async () => {
    log.push(`${name} start`);
    await sleep(ms);
    log.push(`${name} end`);
    return name;
  };
  const far = Date.now() + 10_000;
  const [a, b, c] = await Promise.all([
    queue.run(1, far, job("a", 30)),
    queue.run(1, far, job("b", 5)),
    queue.run(2, far, job("c", 5)), // other tab: not blocked by tab 1
  ]);
  assert.deepEqual([a, b, c], [
    { ran: true, value: "a" },
    { ran: true, value: "b" },
    { ran: true, value: "c" },
  ]);
  assert.ok(log.indexOf("a end") < log.indexOf("b start"), log.join(", "));
  assert.ok(log.indexOf("c end") < log.indexOf("a end"), log.join(", "));
});

test("askControl.queued_ask_past_its_deadline_never_runs", async () => {
  const queue = new TabQueue();
  let secondRan = false;
  let thirdRan = false;
  const first = queue.run(1, Date.now() + 10_000, () => sleep(60));
  const second = queue.run(1, Date.now() + 10, async () => {
    secondRan = true;
  });
  // A later ask still waits for the first one, not just for the expired one.
  const third = queue.run(1, Date.now() + 10_000, async () => {
    thirdRan = true;
    return firstDone;
  });
  let firstDone = false;
  void first.then(() => (firstDone = true));
  assert.deepEqual(await second, { ran: false });
  assert.equal(secondRan, false);
  assert.deepEqual(await third, { ran: true, value: true });
  assert.equal(thirdRan, true);
});

test("askControl.stop_targets_only_the_own_tab_ask", () => {
  const inFlight = new InFlight();
  assert.equal(inFlight.forStop(), null);

  const legacy = inFlight.start({ tabId: 3, hostId: "a1", own: false, contentRequestId: "r1" });
  // A gateway `stop` never targets a CLI ask in a user tab.
  assert.equal(inFlight.forStop(), null);
  const own = inFlight.start({ tabId: 7, hostId: "a2", own: true, contentRequestId: "r2" });
  assert.equal(inFlight.forStop(), own);
  inFlight.finish(own);
  assert.equal(inFlight.forStop(), null);
  inFlight.finish(legacy);
  assert.equal(legacy.cancelled, false);
});

test("askControl.stopActionFor", () => {
  const ask = (submitted: boolean, done = false) => ({ id: "r1", cancelled: false, submitted, done });
  // Not sent yet: cancel, the page is not touched.
  assert.equal(stopActionFor(ask(false), "r1"), "cancel");
  // Sent and running: press Stop for it.
  assert.equal(stopActionFor(ask(true), "r1"), "press_for_ask");
  // The named ask already ended (or is another one): leave the page alone.
  assert.equal(stopActionFor(ask(true, true), "r1"), "ignore");
  assert.equal(stopActionFor(ask(true), "r2"), "ignore");
  assert.equal(stopActionFor(null, "r1"), "ignore");
  // No bridge ask known to the background (e.g. the ask already timed
  // out): stop whatever still generates in the own tab…
  assert.equal(stopActionFor(null, undefined), "press_if_generating");
  assert.equal(stopActionFor(ask(true, true), undefined), "press_if_generating");
  // …or the ask still running there (service worker restarted).
  assert.equal(stopActionFor(ask(true), undefined), "press_for_ask");
  assert.equal(stopActionFor(ask(false), undefined), "cancel");
});

test("askControl.parseHostTarget", () => {
  assert.deepEqual(parseHostTarget(undefined), { ok: true, target: undefined });
  assert.deepEqual(parseHostTarget({ kind: "new_chat" }), { ok: true, target: { kind: "new_chat" } });
  assert.deepEqual(parseHostTarget({ kind: "conversation", url: "https://chatgpt.com/c/abc" }), {
    ok: true,
    target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
  });
  for (const bad of [
    { kind: "chat" },
    { kind: "conversation" },
    { kind: "conversation", url: "https://chatgpt.com/c/local-chatgpt:1234" },
    { kind: "conversation", url: "https://example.com/c/abc" },
    "new_chat",
  ]) {
    assert.equal(parseHostTarget(bad).ok, false, JSON.stringify(bad));
  }
});

test("askControl.only_a_missing_receiver_is_retried", () => {
  assert.equal(
    isReceivingEndMissing("Could not establish connection. Receiving end does not exist."),
    true,
  );
  // The content script took the request and the page went away: never resend.
  assert.equal(
    isReceivingEndMissing(
      "A listener indicated an asynchronous response by returning true, but the message channel closed before a response was received",
    ),
    false,
  );
  assert.equal(isReceivingEndMissing("The message port closed before a response was received."), false);
});

test("askControl.submit_needs_15s_left", () => {
  assert.equal(submitBudgetMs(900_000), 15_000);
  assert.equal(submitBudgetMs(120_000), 15_000);
  assert.equal(submitBudgetMs(10_000), 5_000); // short CLI timeout still sends
});

test("adapter.textAtDeadline", () => {
  const partial = { shimDone: false, shimText: "x".repeat(200), lastSeenText: "half an answer" };
  // Targeted (gateway) asks: a cut-off answer is a timeout, never a success.
  assert.equal(textAtDeadline(partial, false), null);
  assert.equal(textAtDeadline({ shimDone: false, shimText: null, lastSeenText: "half" }, false), null);
  // A stream that completed is a whole answer either way.
  assert.equal(textAtDeadline({ shimDone: true, shimText: "done", lastSeenText: null }, false), "done");
  // Legacy CLI asks keep returning what was there.
  assert.equal(textAtDeadline(partial, true), partial.shimText);
  assert.equal(textAtDeadline({ shimDone: false, shimText: "short", lastSeenText: "dom" }, true), "dom");
  assert.equal(textAtDeadline({ shimDone: false, shimText: null, lastSeenText: null }, true), null);
});

test("adapter.targetMatchesPath", () => {
  const conv = { kind: "conversation", url: "https://chatgpt.com/c/abc" } as const;
  assert.equal(targetMatchesPath(conv, "/c/abc"), true);
  assert.equal(targetMatchesPath(conv, "/c/other"), false);
  assert.equal(targetMatchesPath(conv, "/"), false);
  assert.equal(targetMatchesPath({ kind: "new_chat" }, "/"), true);
  assert.equal(targetMatchesPath({ kind: "new_chat" }, "/c/abc"), false);
  assert.equal(targetMatchesPath({ kind: "new_chat" }, "/c/local-chatgpt:1234"), false);
});

test("askControl.stop_for_the_running_ask_marks_it_cancelled", () => {
  const ask = (submitted: boolean): PageAskState => ({ id: "a1", cancelled: false, submitted, done: false });

  // Not sent yet: it aborts at its next checkpoint.
  const waiting = ask(false);
  assert.equal(takeStop(waiting, "a1"), "cancel");
  assert.equal(waiting.cancelled, true);

  // Sent: Stop is pressed, and the ask may not click Send again either.
  const sent = ask(true);
  assert.equal(takeStop(sent, undefined), "press_for_ask");
  assert.equal(sent.cancelled, true);

  // A stop for another ask leaves this one alone.
  const other = ask(true);
  assert.equal(takeStop(other, "a2"), "ignore");
  assert.equal(other.cancelled, false);

  const over = { ...ask(true), done: true };
  assert.equal(takeStop(over, undefined), "press_if_generating");
  assert.equal(over.cancelled, false);
});

test("askControl.a_named_stop_finds_its_ask_in_any_tab_and_nothing_else", () => {
  const inFlight = new InFlight();
  const cli = inFlight.start({ tabId: 3, hostId: "a1", own: false, contentRequestId: "r1" });
  const own = inFlight.start({ tabId: 7, hostId: "a2", own: true, contentRequestId: "r2" });
  assert.equal(inFlight.named("a1"), cli);
  assert.equal(inFlight.named("a2"), own);
  assert.equal(inFlight.named("r1"), null, "the content request id is not the host's");
  assert.equal(inFlight.named("gone"), null);
  inFlight.finish(cli);
  assert.equal(inFlight.named("a1"), null);
  inFlight.finish(own);
});

test("askControl.own_asks_lists_the_running_and_the_queued_own_tab_asks_only", () => {
  const inFlight = new InFlight();
  const legacy = inFlight.start({ tabId: 3, hostId: "a1", own: false, contentRequestId: "r1" });
  const running = inFlight.start({ tabId: 7, hostId: "a2", own: true, contentRequestId: "r2" });
  const queued = inFlight.start({ tabId: 7, hostId: "a3", own: true, contentRequestId: "r3" });
  assert.deepEqual(inFlight.ownAsks(), [running, queued]);
  assert.equal(inFlight.forStop(), running, "the stop names the one that runs");
  inFlight.finish(running);
  assert.deepEqual(inFlight.ownAsks(), [queued]);
  inFlight.finish(queued);
  inFlight.finish(legacy);
  assert.deepEqual(inFlight.ownAsks(), []);
});

test("askControl.a_stop_prefers_the_own_ask_that_has_its_tab_over_one_still_choosing", () => {
  const inFlight = new InFlight();
  const choosing = inFlight.start({ tabId: null, hostId: "a1", own: true, contentRequestId: "r1" });
  assert.equal(choosing.handedToTab, false);
  assert.equal(inFlight.forStop(), choosing, "the only own ask, tab or not");
  const running = inFlight.start({ tabId: 7, hostId: "a2", own: true, contentRequestId: "r2" });
  assert.equal(inFlight.forStop(), running);
  assert.equal(inFlight.named("a1"), choosing, "a stop_ask finds it before its tab is known");
  inFlight.finish(running);
  inFlight.finish(choosing);
});
