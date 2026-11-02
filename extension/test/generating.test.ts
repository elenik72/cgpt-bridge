// How the adapter tells "ChatGPT is generating", presses Stop and confirms a
// Send click, against a fake page: just enough of `document` for the probes,
// elements keyed by the selector that finds them. Live 26.10.2026 a hidden
// Stop-like button kept `isGenerating()` true for minutes while
// `findStopButton()` found nothing.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  assertAtTarget,
  describeDom,
  isGenerating,
  isRateLimitSignal,
  openTarget,
  stopGenerating,
  submit,
  waitForSavedChatUrl,
  summarizeDiagnostics,
  type AdapterDiagnostics,
} from "../src/chatgptAdapter";
import { tabLoadTimeoutMessage, takeStop, type PageAskState } from "../src/askControl";
import { CancelledError } from "../src/errors";
import { useAutoClock } from "./harness/clock";

interface FakeElement {
  disabled: boolean;
  hidden: boolean;
  visible: boolean;
  dataset: Record<string, string>;
  attrs: Record<string, string>;
  // Text of a composer.
  text?: string;
  clicks: number;
  onClick?: () => void;
}

const page = new Map<string, FakeElement[]>();
const fakeLocation = { host: "chatgpt.com", href: "https://chatgpt.com/c/abc", pathname: "/c/abc" };

function goTo(pathname: string): void {
  fakeLocation.pathname = pathname;
  fakeLocation.href = `https://chatgpt.com${pathname}`;
}

function element(opts: Partial<FakeElement> = {}): FakeElement {
  return {
    disabled: false,
    hidden: false,
    visible: true,
    dataset: {},
    attrs: {},
    clicks: 0,
    ...opts,
  };
}

// The DOM surface the adapter touches on a button or turn node.
function asNode(el: FakeElement): unknown {
  return {
    get disabled() {
      return el.disabled;
    },
    get hidden() {
      return el.hidden;
    },
    get dataset() {
      return el.dataset;
    },
    get offsetParent() {
      return el.visible ? {} : null;
    },
    getBoundingClientRect: () => (el.visible ? { width: 10, height: 10 } : { width: 0, height: 0 }),
    getAttribute: (name: string) => el.attrs[name] ?? null,
    hasAttribute: (name: string) => name in el.attrs,
    get textContent() {
      return el.text ?? "";
    },
    closest: () => null,
    click: () => {
      el.clicks++;
      el.onClick?.();
    },
  };
}

function installFakePage(): void {
  const all = (selector: string): unknown[] =>
    selector
      .split(",")
      .flatMap((s) => page.get(s.trim()) ?? [])
      .map(asNode);
  const g = globalThis as Record<string, unknown>;
  g.document = {
    querySelector: (s: string) => all(s)[0] ?? null,
    querySelectorAll: (s: string) => all(s),
    documentElement: {},
    body: {},
  };
  g.location = fakeLocation;
  g.window = fakeWindow;
  g.HTMLTextAreaElement = class {};
  g.MutationObserver = class {
    observe(): void {}
    disconnect(): void {}
  };
}

// `window` as far as the adapter's `message` listeners go (pageShim posts).
const messageListeners = new Set<(ev: unknown) => void>();
const fakeWindow = {
  addEventListener: (type: string, fn: (ev: unknown) => void) => {
    if (type === "message") messageListeners.add(fn);
  },
  removeEventListener: (type: string, fn: (ev: unknown) => void) => {
    if (type === "message") messageListeners.delete(fn);
  },
};

function postFromShim(data: Record<string, unknown>): void {
  for (const fn of [...messageListeners]) fn({ source: fakeWindow, data: { __cgptBridge: true, ...data } });
}

installFakePage();
useAutoClock();

const STOP_BY_LABEL = 'button[aria-label*="Stop" i]';
const TURN = "[data-talvt-turn-state]";

function reset(): void {
  page.clear();
  goTo("/c/abc");
}

test("generating.hidden_or_disabled_stop_like_button_is_not_generating", () => {
  reset();
  page.set(STOP_BY_LABEL, [
    element({ visible: false, attrs: { "aria-label": "Stop sharing" } }),
    element({ disabled: true, attrs: { "aria-label": "Stop" } }),
  ]);
  assert.equal(isGenerating(), false);
  assert.match(describeDom(), /generating=false signal=none/);

  // A usable one behind them counts, and is the one pressed.
  const usable = element({ attrs: { "aria-label": "Stop streaming" } });
  page.get(STOP_BY_LABEL)?.push(usable);
  assert.equal(isGenerating(), true);
  assert.match(describeDom(), /generating=true signal=stop-selector:Stop streaming/);
});

test("generating.stop_presses_the_usable_button", async () => {
  reset();
  const hidden = element({ visible: false, attrs: { "aria-label": "Stop" } });
  const usable = element({ attrs: { "aria-label": "Stop" } });
  page.set(STOP_BY_LABEL, [hidden, usable]);
  setTimeout(() => page.delete(STOP_BY_LABEL), 50); // the answer stops
  assert.equal(await stopGenerating(2_000, 0), true);
  assert.equal(usable.clicks, 1);
  assert.equal(hidden.clicks, 0);
});

test("generating.turn_in_progress_without_stop_waits_for_it_to_clear", async () => {
  reset();
  const turn = element({ dataset: { talvtTurnState: "in_progress" } });
  page.set(TURN, [turn]);
  page.set(STOP_BY_LABEL, [element({ visible: false, attrs: { "aria-label": "Stop" } })]);
  assert.match(describeDom(), /signal=turn-in_progress/);
  setTimeout(() => (turn.dataset["talvtTurnState"] = "complete"), 300);
  const started = Date.now();
  // Nothing to press, and the answer ended by itself: not an error.
  assert.equal(await stopGenerating(3_000, 0), false);
  assert.ok(Date.now() - started >= 250, "it waited for the turn to finish");
});

test("generating.turn_stuck_in_progress_fails_with_the_signal_and_path", async () => {
  reset();
  page.set(TURN, [element({ dataset: { talvtTurnState: "in_progress" } })]);
  await assert.rejects(stopGenerating(400, 0), (err: Error) => {
    assert.equal(err.name, "StopButtonNotFoundError");
    assert.match(err.message, /generating=true signal=turn-in_progress/);
    assert.match(err.message, /path=\/c\/abc/);
    return true;
  });
});

test("generating.diagnostics_summary_names_signal_path_and_composer_text", () => {
  const d = {
    uiProfile: "account",
    pathname: "/",
    composerFound: true,
    composerTextLen: 42,
    sendButtonFound: true,
    isGenerating: false,
    generatingSignal: null,
    assistantTurnCount: 0,
  } as unknown as AdapterDiagnostics;
  assert.equal(
    summarizeDiagnostics(d),
    "ui=account composer=true composerText=42 send=true generating=false signal=none answers=0 path=/",
  );
});

test("askControl.tab_load_timeout_says_what_it_waited_for", () => {
  assert.equal(
    tabLoadTimeoutMessage({
      timeoutMs: 60_000,
      waitingFor: "https://chatgpt.com/",
      tab: { status: "loading", url: "https://chatgpt.com/c/abc" },
      page: "ui=account composer=true",
    }),
    "the ChatGPT tab did not load within 60000 ms (waiting for https://chatgpt.com/; " +
      "tab status=loading url=https://chatgpt.com/c/abc; page: ui=account composer=true)",
  );
  assert.equal(
    tabLoadTimeoutMessage({
      timeoutMs: 60_000,
      waitingFor: "the current page",
      tab: null,
      page: null,
    }),
    "the ChatGPT tab did not load within 60000 ms (waiting for the current page; " +
      "tab gone; page: no answer)",
  );
  // A sign-in redirect keeps its tokens in the query: they stay out.
  assert.equal(
    tabLoadTimeoutMessage({
      timeoutMs: 60_000,
      waitingFor: "https://chatgpt.com/",
      tab: { status: "loading", url: "https://auth.openai.com/authorize?state=s#x", pendingUrl: "https://chatgpt.com/?code=c" },
      page: null,
    }),
    "the ChatGPT tab did not load within 60000 ms (waiting for https://chatgpt.com/; " +
      "tab status=loading url=https://auth.openai.com/authorize pendingUrl=https://chatgpt.com/; page: no answer)",
  );
});

test("chatUrl.targeted_ask_without_a_saved_address_fails_naming_the_path", async () => {
  reset();
  goTo("/c/local-chatgpt:1234");
  await assert.rejects(waitForSavedChatUrl(300, { kind: "new_chat" }), (err: Error & { code?: string }) => {
    assert.equal(err.code, "chat_url_missing");
    assert.match(err.message, /path=\/c\/local-chatgpt:1234/);
    return true;
  });
  // The temporary path turning into the saved one in time is fine.
  setTimeout(() => goTo("/c/saved"), 100);
  assert.equal(await waitForSavedChatUrl(2_000, { kind: "new_chat" }), "https://chatgpt.com/c/saved");
});

test("openTarget.new_chat_switched_in_page_settles_before_typing", async () => {
  reset();
  let clickedAt = 0;
  page.set("button[data-sidebar-cycle-item]", [
    element({
      onClick: () => {
        clickedAt = Date.now();
        goTo("/");
      },
    }),
  ]);
  page.set(
    'div.ProseMirror[contenteditable="true"][role="textbox"][data-composer-markdown]',
    [element()],
  );
  const opened = await openTarget({ kind: "new_chat" }, { deadline: Date.now() + 10_000, reloaded: false });
  assert.deepEqual(opened, { ready: true, target: { kind: "new_chat" } });
  assert.ok(clickedAt > 0, "New chat was clicked");
  assert.ok(Date.now() - clickedAt >= 450, `ready ${Date.now() - clickedAt} ms after the switch`);
});

const COMPOSER = 'div.ProseMirror[contenteditable="true"][role="textbox"][data-composer-markdown]';
const SEND = "button[data-composer-submit]";

// A composer that keeps showing the prompt after a Send click (as in the
// live case of 26.10.2026) and the Send button. Nothing else happens on a
// click unless the test says so.
function promptStaysInComposer(): { composer: FakeElement; send: FakeElement } {
  const composer = element({ text: "the prompt" });
  const send = element();
  page.set(COMPOSER, [composer]);
  page.set(SEND, [send]);
  return { composer, send };
}

test("submit.new_chat_that_moved_to_its_address_counts_as_sent", async () => {
  reset();
  goTo("/");
  const { send } = promptStaysInComposer();
  send.onClick = () => goTo("/c/local-chatgpt:1234");
  let checks = 0;
  // The content script checks the target before every click: a second
  // click would fail "left the target chat before the prompt was sent".
  await submit(() => {
    checks++;
    assertAtTarget({ kind: "new_chat" });
  });
  assert.equal(send.clicks, 1);
  assert.equal(checks, 1);
});

test("submit.new_user_turn_counts_as_sent", async () => {
  reset();
  page.set(TURN, [element({ dataset: { talvtTurnState: "complete" } })]);
  const { send } = promptStaysInComposer();
  send.onClick = () => page.get(TURN)?.push(element({ dataset: { talvtTurnState: "complete" } }));
  await submit();
  assert.equal(send.clicks, 1);
});

test("submit.answer_stream_start_counts_as_sent", async () => {
  reset();
  const { send } = promptStaysInComposer();
  send.onClick = () => setTimeout(() => postFromShim({ kind: "sse-start", text: "", epoch: 7 }), 10);
  await submit(undefined, 7);
  assert.equal(send.clicks, 1);
  assert.equal(messageListeners.size, 0, "the stream listener is removed");
});

// A stream of an earlier shim epoch (an answer still streaming from a
// previous ask), or a report without an epoch (a shim older than the
// content script), says nothing about this click.
test("submit.stream_start_of_another_epoch_does_not_count_as_sent", async () => {
  reset();
  const { composer, send } = promptStaysInComposer();
  send.onClick = () => {
    if (send.clicks === 2) composer.text = "";
    setTimeout(() => {
      postFromShim({ kind: "sse-start", text: "", epoch: 6 });
      postFromShim({ kind: "sse-start", text: "" });
    }, 10);
  };
  await submit(undefined, 7);
  assert.equal(send.clicks, 2, "the click was confirmed by another stream");
  assert.equal(messageListeners.size, 0);
});

// A 429 on the answer stream is an answer to the click: Send is not pressed
// again while the prompt is still in the composer.
test("submit.rate_limited_stream_is_not_followed_by_another_click", async () => {
  reset();
  const { send } = promptStaysInComposer();
  send.onClick = () => setTimeout(() => postFromShim({ kind: "sse-error", status: 429, epoch: 7 }), 10);
  const reports = await submit(undefined, 7);
  assert.equal(send.clicks, 1);
  assert.equal(reports.some((r) => isRateLimitSignal(r)), true);
  assert.equal(messageListeners.size, 0);
});

// An empty answer may start and end before the click is confirmed: submit
// hands the reports of its own epoch, in order, to the answer wait.
test("submit.returns_the_stream_reports_of_its_epoch_seen_since_the_click", async () => {
  reset();
  const { send } = promptStaysInComposer();
  send.onClick = () =>
    setTimeout(() => {
      postFromShim({ kind: "sse-done", text: "old answer", epoch: 6 });
      postFromShim({ kind: "sse-done", text: "no epoch" });
      postFromShim({ kind: "sse-start", text: "", epoch: 7 });
      postFromShim({ kind: "sse-done", text: "", epoch: 7 });
    }, 10);
  const reports = await submit(undefined, 7);
  assert.deepEqual(
    reports.map((r) => [r.kind, r.text, r.epoch]),
    [
      ["sse-start", "", 7],
      ["sse-done", "", 7],
    ],
  );
  assert.equal(send.clicks, 1);
  assert.equal(messageListeners.size, 0);
});

// A report of this epoch posted before the click (here while submit waits
// for the Send button) is not this prompt's answer: neither returned nor a
// send sign.
test("submit.stream_reports_before_the_click_are_not_returned", async () => {
  reset();
  const composer = element({ text: "the prompt" });
  const send = element();
  page.set(COMPOSER, [composer]);
  setTimeout(() => {
    postFromShim({ kind: "sse-start", text: "", epoch: 7 });
    postFromShim({ kind: "sse-done", text: "before the click", epoch: 7 });
  }, 50);
  setTimeout(() => page.set(SEND, [send]), 300);
  // The first click is confirmed by nothing (its stream's report is all
  // that comes); the second one empties the composer.
  send.onClick = () => {
    if (send.clicks === 2) composer.text = "";
    else setTimeout(() => postFromShim({ kind: "sse-done", text: "after the click", epoch: 7 }), 10);
  };
  const reports = await submit(undefined, 7);
  assert.equal(send.clicks, 2, "the stream that started before the click confirmed it");
  assert.deepEqual(
    reports.map((r) => [r.kind, r.text]),
    [["sse-done", "after the click"]],
  );
  assert.equal(messageListeners.size, 0);
});

test("submit.click_the_page_dropped_is_repeated", async () => {
  reset();
  const { composer, send } = promptStaysInComposer();
  send.onClick = () => {
    if (send.clicks === 2) composer.text = "";
  };
  let checks = 0;
  await submit(() => {
    checks++;
  });
  assert.equal(send.clicks, 2);
  assert.equal(checks, 2);
});

test("submit.no_second_click_after_a_stop", async () => {
  reset();
  const { send } = promptStaysInComposer();
  const ask: PageAskState = { id: "a1", cancelled: false, submitted: false, done: false };
  send.onClick = () => {
    // Stop arrives while the click is being confirmed.
    setTimeout(() => assert.equal(takeStop(ask, "a1"), "press_for_ask"), 50);
  };
  await assert.rejects(
    submit(() => {
      if (ask.cancelled) throw new CancelledError();
      ask.submitted = true;
    }),
    (err: Error) => err.name === "CancelledError",
  );
  assert.equal(send.clicks, 1);
  assert.equal(messageListeners.size, 0);
});
