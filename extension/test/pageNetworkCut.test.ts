// L2 on the fake window: the answer stream breaks off mid-answer with no
// HTTP status (live 28.10, research/2026-10-28-live-sse-and-markup.md: a
// network cut ends the read with `TypeError: network error`, no
// `message_stream_complete`, no `[DONE]`; the server finishes the answer and
// the page shows all of it once the network is back). The ask then waits for
// the page's own answer, within its deadline. A 429 stays `rate_limited`
// (page.http_429_* in page.test.ts).
//
// What the page shows during a cut was not recorded. The worst case for the
// adapter is assumed: the partial text, no Stop, and a turn state it does
// not know ("interrupted" below is made up), so nothing reads as generating.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage, sendToContent, shimReports } from "./harness/fakeWindow";
import { AccountChat, later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const CHAT = "https://chatgpt.com/c/abc";
const CUT_AT_MS = 200;
// How long the network stays down (live: 5 s).
const OUTAGE_MS = 5_000;
// A failed gateway turn's Stop first waits this long for generation to show
// (content.ts ABANDON_START_GRACE_MS); the error comes after it.
const ABANDON_GRACE_MS = 1_000;

/**
 * Send starts a turn whose stream and DOM show "Part one", then the stream
 * is cut and the page shows that text as if nothing were running.
 */
function cutMidAnswer(chat: AccountChat, onCut: (turn: ReturnType<AccountChat["beginTurn"]>) => void = () => {}): {
  stream: () => AnswerStream | null;
} {
  let stream = null as AnswerStream | null;
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const s = pageFetchesAnswerStream();
    stream = s;
    later(50, () => {
      s.startAnswer("m1", "Part one");
      turn.render("Part one");
    });
    later(CUT_AT_MS, () => {
      s.cut();
      turn.el.setAttribute("data-talvt-turn-state", "interrupted");
      chat.showSend();
      onCut(turn);
    });
  };
  return { stream: () => stream };
}

test("networkCut.page_recovers_the_answer_and_the_ask_takes_it", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  const cut = cutMidAnswer(chat, (turn) => {
    later(OUTAGE_MS, () => {
      turn.render("Part one.\n\nPart two, the rest of the answer.");
      turn.finish();
    });
  });
  try {
    const started = Date.now();
    const reply = await ask("cut1", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 30_000 });
    assert.deepEqual(reply, {
      id: "cut1",
      ok: true,
      kind: "test.ask",
      text: "Part one.\nPart two, the rest of the answer.",
      conversationUrl: CHAT,
    });
    const reports = shimReports();
    assert.equal(reports.at(-1)?.kind, "sse-error");
    assert.equal(reports.at(-1)?.status, undefined);
    // Not the partial text the page showed during the cut, and not the deadline.
    const took = Date.now() - started;
    assert.ok(took >= CUT_AT_MS + OUTAGE_MS && took < 10_000, `answered after ${took} ms`);
  } finally {
    await settlePage([cut.stream()]);
  }
});

// The page may resume through the same endpoint, in the same shim epoch. A
// resumed stream that carries only the rest of the answer, with its own
// completion, must not answer: after a cut only the page's DOM does.
test("networkCut.resumed_stream_with_only_the_rest_does_not_answer", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let resumed = null as AnswerStream | null;
  const cut = cutMidAnswer(chat, (turn) => {
    later(OUTAGE_MS, () => {
      const s = pageFetchesAnswerStream();
      resumed = s;
      s.startAnswer("m1", "Part two, the rest of the answer.");
      s.endTurn();
      s.done();
    });
    later(OUTAGE_MS + 300, () => {
      turn.render("Part one.\n\nPart two, the rest of the answer.");
      turn.finish();
    });
  });
  try {
    const reply = await ask("cut5", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 30_000 });
    assert.equal(reply.text, "Part one.\nPart two, the rest of the answer.", JSON.stringify(reply));
    assert.ok(shimReports().some((r) => r.kind === "sse-done" && r.text === "Part two, the rest of the answer."));
  } finally {
    await settlePage([cut.stream(), resumed]);
  }
});

// A resumed stream with no answer text must not hand back the partial text
// the DOM showed during the cut.
test("networkCut.empty_resumed_stream_does_not_answer_with_the_partial_text", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let resumed = null as AnswerStream | null;
  const cut = cutMidAnswer(chat, (turn) => {
    later(OUTAGE_MS, () => {
      const s = pageFetchesAnswerStream();
      resumed = s;
      s.event({ type: "title_generation", title: "Cities" });
      s.done();
    });
    later(OUTAGE_MS + 300, () => {
      turn.render("Part one.\n\nPart two, the rest of the answer.");
      turn.finish();
    });
  });
  try {
    const reply = await ask("cut6", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 30_000 });
    assert.equal(reply.text, "Part one.\nPart two, the rest of the answer.", JSON.stringify(reply));
  } finally {
    await settlePage([cut.stream(), resumed]);
  }
});

// The page shows the partial text under `complete` during the outage and
// never changes it: not an answer, the ask fails at its deadline.
test("networkCut.complete_turn_on_the_unchanged_partial_text_is_no_answer", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const s = pageFetchesAnswerStream();
    stream = s;
    later(50, () => {
      s.startAnswer("m1", "Part one");
      turn.render("Part one");
    });
    later(CUT_AT_MS, () => {
      s.cut();
      turn.finish();
    });
  };
  try {
    const started = Date.now();
    const reply = await ask("cut7", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 8_000 });
    assert.equal(reply.errorCode, "answer_timeout", JSON.stringify(reply));
    const took = Date.now() - started;
    assert.ok(took >= 7_500 && took <= 8_000 + ABANDON_GRACE_MS + 500, `failed after ${took} ms`);
  } finally {
    await settlePage([stream]);
  }
});

test("networkCut.no_answer_after_the_cut_fails_at_the_deadline", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  // The page never gets the rest: the partial text stays.
  const cut = cutMidAnswer(chat);
  try {
    const started = Date.now();
    const reply = await ask("cut2", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 8_000 });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "answer_timeout");
    const took = Date.now() - started;
    assert.ok(took >= 7_500 && took <= 8_000 + ABANDON_GRACE_MS + 500, `failed after ${took} ms`);
  } finally {
    await settlePage([cut.stream()]);
  }
});

test("networkCut.no_dom_answer_after_the_cut_fails_at_the_deadline", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  chat.onSend = () => {
    // The page renders nothing of the answer; only the stream had begun.
    const turn = chat.beginTurn();
    const s = pageFetchesAnswerStream();
    stream = s;
    later(50, () => s.startAnswer("m1", "Part one"));
    later(CUT_AT_MS, () => {
      s.cut();
      turn.el.setAttribute("data-talvt-turn-state", "interrupted");
      chat.showSend();
    });
  };
  try {
    const started = Date.now();
    const reply = await ask("cut3", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 8_000 });
    assert.equal(reply.errorCode, "answer_timeout", JSON.stringify(reply));
    const took = Date.now() - started;
    assert.ok(took >= 7_500 && took <= 8_000 + ABANDON_GRACE_MS + 500, `failed after ${took} ms`);
  } finally {
    await settlePage([stream]);
  }
});

// A Stop from the host: the page aborts its fetch (the read ends with an
// AbortError, no status) and marks the turn `cancelled`. The ask ends at
// once, "cancelled", as before.
test("networkCut.stop_that_aborts_the_stream_still_ends_the_ask_at_once", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  let turn = null as ReturnType<AccountChat["beginTurn"]> | null;
  chat.onSend = () => {
    const t = chat.beginTurn();
    turn = t;
    const s = pageFetchesAnswerStream();
    stream = s;
    later(50, () => {
      s.startAnswer("m1", "Part one");
      t.render("Part one");
    });
    later(300, () => void sendToContent({ id: "cut4-stop", type: "stop", askId: "cut4" }));
  };
  chat.stop.onClick = () => {
    stream?.cut(new DOMException("The user aborted a request.", "AbortError"));
    turn?.finish("cancelled");
  };
  try {
    const started = Date.now();
    const reply = await ask("cut4", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 30_000 });
    assert.equal(reply.errorCode, "cancelled", JSON.stringify(reply));
    assert.equal(chat.stop.clicks, 1);
    const took = Date.now() - started;
    assert.ok(took < 5_000, `ended after ${took} ms`);
  } finally {
    await settlePage([stream]);
  }
});

// Live 30.10 (TASK-056): after a mid-answer Stop the turn reads `complete`,
// not `cancelled`, and the page aborts the stream after that. The abort is
// not a network cut: the ask ends at once, "cancelled", instead of waiting
// for an answer that will not grow, to its deadline (holding the gateway).
test("networkCut.stop_whose_turn_reads_complete_ends_the_ask_at_once", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  let turn = null as ReturnType<AccountChat["beginTurn"]> | null;
  chat.onSend = () => {
    const t = chat.beginTurn();
    turn = t;
    const s = pageFetchesAnswerStream();
    stream = s;
    later(50, () => {
      s.startAnswer("m1", "Part one");
      t.render("Part one");
    });
    later(300, () => void sendToContent({ id: "cut5-stop", type: "stop", askId: "cut5" }));
  };
  chat.stop.onClick = () => {
    turn?.finish("complete");
    later(50, () => stream?.cut(new DOMException("The user aborted a request.", "AbortError")));
  };
  try {
    const started = Date.now();
    const reply = await ask("cut5", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 30_000 });
    assert.equal(reply.errorCode, "cancelled", JSON.stringify(reply));
    assert.equal(chat.stop.clicks, 1);
    const took = Date.now() - started;
    assert.ok(took < 5_000, `ended after ${took} ms`);
  } finally {
    await settlePage([stream]);
  }
});

// Live 30.10 (TASK-056), after the fix above: the page read `cancelled`
// with nothing running and the ask still waited to its deadline — what the
// page shows of a stopped answer is not known. Here the stopped turn is
// gone from the page altogether: the ask still ends soon after the Stop.
test("networkCut.stop_ends_the_ask_whatever_the_page_shows", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  let turn = null as ReturnType<AccountChat["beginTurn"]> | null;
  chat.onSend = () => {
    const t = chat.beginTurn();
    turn = t;
    const s = pageFetchesAnswerStream();
    stream = s;
    later(50, () => {
      s.startAnswer("m1", "Part one");
    });
    later(300, () => void sendToContent({ id: "cut6-stop", type: "stop", askId: "cut6" }));
  };
  chat.stop.onClick = () => {
    turn?.el.remove();
    chat.showSend();
  };
  try {
    const started = Date.now();
    const reply = await ask("cut6", "question", { target: { kind: "conversation", url: CHAT }, timeoutMs: 60_000 });
    assert.equal(reply.errorCode, "cancelled", JSON.stringify(reply));
    assert.equal(chat.stop.clicks, 1);
    const took = Date.now() - started;
    assert.ok(took < 5_000, `ended after ${took} ms`);
  } finally {
    await settlePage([stream]);
  }
});
