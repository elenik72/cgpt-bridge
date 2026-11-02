// The page end to end, in the fake window (test/harness): the real
// pageShim.ts and content.ts, a fake ChatGPT page, a request from the
// background. These pin today's happy paths; the audit's failure
// reproductions are in pageAudit-*.test.ts (one probe per file).

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  ask,
  goTo,
  h,
  loadPage,
  routeFetch,
  shimActive,
  shimReports,
  until,
  win,
  windowListenerCount,
} from "./harness/fakeWindow";
import {
  AccountChat,
  CONVERSATION_URL,
  later,
  pageFetchesAnswerStream,
  pageIdle,
  type AccountTurn,
  type AnswerStream,
} from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const CHAT = "https://chatgpt.com/c/abc";

// After every ask: the shim goes back to sleep and the adapter drops its
// listeners, so only the shim's own `message` listener stays. content.ts
// replies before the shim's last "off" is acknowledged, hence the waits.
async function assertPageAtRest(): Promise<void> {
  await pageIdle();
  await until(() => shimActive() === false, "the shim to go dormant");
  await until(() => windowListenerCount("message") === 1, "only the shim to listen for messages");
}

test("page.own_stream_answers_a_gateway_ask", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const s = pageFetchesAnswerStream();
    stream = s;
    later(50, () => {
      s.startAnswer("m1");
      s.append("Here is **the** answer");
      // The DOM shows a rendering of the markdown, and lags behind.
      turn.render("Here is the");
    });
    later(150, () => {
      s.append(" from the stream.");
      s.endTurn();
    });
    later(400, () => {
      turn.render("Here is the answer from the stream.");
      turn.finish();
      s.done();
    });
  };

  const reply = await ask("r1", "hello", { target: { kind: "conversation", url: CHAT } });

  assert.deepEqual(reply, {
    id: "r1",
    ok: true,
    kind: "test.ask",
    // The stream's source markdown, not the DOM rendering.
    text: "Here is **the** answer from the stream.",
    conversationUrl: CHAT,
  });
  assert.deepEqual(chat.sentPrompts, ["hello"]);
  assert.equal(chat.send.clicks, 1);
  const kinds = shimReports().map((r) => r.kind);
  assert.equal(kinds[0], "sse-start");
  assert.equal(kinds.filter((k) => k === "sse-done").length, 1);
  await assertPageAtRest();
  // The shim tees the body: the page still reads all of it.
  await until(() => stream?.pageRead.endsWith("data: [DONE]\n\n") === true, "the page to read the whole stream");
});

test("page.dom_answers_a_new_chat_without_a_stream", async () => {
  const chat = new AccountChat("/");
  chat.onSend = () => {
    // A new chat moves to its temporary address on submit, then to the saved one.
    goTo("/c/local-chatgpt:5f0c");
    const turn = chat.beginTurn();
    later(100, () => turn.render("Answer rendered"));
    later(300, () => {
      turn.render("Answer rendered in the DOM.\n\nSecond paragraph.");
      turn.finish();
    });
    later(600, () => goTo("/c/saved-1"));
  };

  const started = Date.now();
  const reply = await ask("r2", "new question", { target: { kind: "new_chat" } });

  assert.deepEqual(reply, {
    id: "r2",
    ok: true,
    kind: "test.ask",
    text: "Answer rendered in the DOM.\nSecond paragraph.",
    conversationUrl: "https://chatgpt.com/c/saved-1",
  });
  assert.deepEqual(chat.sentPrompts, ["new question"]);
  // No stream: the answer is taken once the DOM held still (800 ms).
  assert.equal(shimReports().length, 0);
  assert.ok(Date.now() - started >= 1_000, `answered after ${Date.now() - started} ms`);
  await assertPageAtRest();
});

test("page.saved_chat_that_does_not_open_after_a_load_is_target_not_reached_when_signed_in", async () => {
  // Signed in, but the load of the chat ended on the start page.
  const chat = new AccountChat("/");
  const reply = await ask("r4", "question", {
    target: { kind: "conversation", url: "https://chatgpt.com/c/gone" },
    reloaded: true,
  });
  assert.equal(reply.errorCode, "target_not_reached", JSON.stringify(reply));
  assert.deepEqual(chat.sentPrompts, []);
  // Refused before the shim was woken.
  await pageIdle();
});

test("page.signed_in_page_still_showing_the_signed_out_shell_gets_the_prompt", async () => {
  const chat = new AccountChat("/");
  // Still loading: a composer is there, but it is the signed-out markup's.
  chat.composer.remove();
  const shell = h("div", {
    sels: ["[data-composer-editor-host]"],
    children: [h("textarea", { sels: ['textarea[name="prompt"]'], attrs: { name: "prompt" } })],
  });
  chat.main.append(shell);
  later(300, () => {
    shell.remove();
    chat.form.replaceChildren(chat.composer, chat.send);
  });
  chat.onSend = () => {
    goTo("/c/local-chatgpt:7a1d");
    const turn = chat.beginTurn();
    later(100, () => {
      turn.render("Signed-in answer.");
      turn.finish();
    });
    later(300, () => goTo("/c/saved-2"));
  };

  const reply = await ask("r3", "after load", { target: { kind: "new_chat" } });

  assert.equal(reply.ok, true, JSON.stringify(reply));
  assert.deepEqual(chat.sentPrompts, ["after load"]);
  assert.equal(reply.text, "Signed-in answer.");
  await assertPageAtRest();
});

test("page.cli_ask_in_an_idle_tab", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const stream = pageFetchesAnswerStream();
    later(50, () => {
      stream.startAnswer("m1", "CLI ");
      turn.render("CLI");
    });
    later(200, () => {
      stream.append("answer");
      stream.endTurn();
      turn.render("CLI answer");
      turn.finish();
      stream.done();
    });
  };

  // No target: the tab is used as it is, nothing is opened or stopped first.
  const reply = await ask("r3", "line one\nline two");

  assert.deepEqual(reply, { id: "r3", ok: true, kind: "test.ask", text: "CLI answer", conversationUrl: CHAT });
  // Lines reach the composer as separate paragraphs.
  assert.deepEqual(chat.sentPrompts, ["line one\nline two"]);
  await assertPageAtRest();
});

// The stream ends with no assistant text (`title_generation`, `[DONE]`) and
// the answer shows up in the DOM a second later: the DOM text is the answer.
// Guards TASK-015, whose fast failure must leave this case alone.
test("page.dom_answer_wins_after_an_empty_stream", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const stream = pageFetchesAnswerStream();
    later(100, () => {
      stream.event({ type: "title_generation", title: "Greeting" });
      stream.done();
    });
    later(1_000, () => {
      turn.render("Answer only the DOM has.");
      turn.finish();
    });
  };

  const reply = await ask("r4", "hello", { target: { kind: "conversation", url: CHAT } });

  assert.deepEqual(reply, { id: "r4", ok: true, kind: "test.ask", text: "Answer only the DOM has.", conversationUrl: CHAT });
  assert.deepEqual(
    shimReports().map((r) => [r.kind, r.text]),
    [
      ["sse-start", ""],
      ["sse-done", ""],
    ],
  );
  await assertPageAtRest();
});

// The same, but the page stopped generating when the stream ended and the
// answer reaches the DOM 2 s later: still inside the 3 s grace, so the DOM
// text is the answer and the fast failure of TASK-015 does not fire.
test("page.dom_answer_inside_the_grace_wins_after_an_empty_stream", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const stream = pageFetchesAnswerStream();
    later(100, () => {
      stream.event({ type: "title_generation", title: "Greeting" });
      stream.done();
      turn.finish();
    });
    later(2_100, () => turn.render("Late answer only the DOM has."));
  };

  const reply = await ask("r4b", "hello", { target: { kind: "conversation", url: CHAT } });

  assert.deepEqual(reply, { id: "r4b", ok: true, kind: "test.ask", text: "Late answer only the DOM has.", conversationUrl: CHAT });
  await assertPageAtRest();
});

// The stream ended without answer text but the page keeps generating for 4 s
// and renders the answer half a second after that: the grace counts from the
// end of "generating", not from the end of the stream, so the DOM text wins.
test("page.dom_answer_after_a_long_generation_wins_after_an_empty_stream", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const stream = pageFetchesAnswerStream();
    later(100, () => {
      stream.event({ type: "title_generation", title: "Greeting" });
      stream.done();
    });
    later(4_100, () => turn.finish());
    later(4_600, () => turn.render("Answer after a long generation."));
  };

  const reply = await ask("r4c", "hello", { target: { kind: "conversation", url: CHAT } });

  assert.deepEqual(reply, { id: "r4c", ok: true, kind: "test.ask", text: "Answer after a long generation.", conversationUrl: CHAT });
  await assertPageAtRest();
});

// After an empty stream the page opens a second one in the same ask, which
// stays silent past the grace (the page shows no "generating") and then
// answers: a new stream restarts the wait, so its answer is taken.
test("page.a_new_stream_after_an_empty_one_restarts_the_wait", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  const streams: AnswerStream[] = [];
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      const first = pageFetchesAnswerStream();
      streams.push(first);
      later(100, () => {
        first.event({ type: "title_generation", title: "Greeting" });
        first.done();
        turn.finish();
      });
      later(2_000, () => {
        const second = pageFetchesAnswerStream();
        streams.push(second);
        later(2_500, () => {
          second.startAnswer("m1", "Answer from the second stream.");
          second.endTurn();
          second.done();
        });
      });
    };

    const reply = await ask("r4d", "hello", { target: { kind: "conversation", url: CHAT } });

    assert.deepEqual(reply, { id: "r4d", ok: true, kind: "test.ask", text: "Answer from the second stream.", conversationUrl: CHAT });
  } finally {
    for (const s of streams) s.close();
    await assertPageAtRest();
  }
});

// After an empty stream the page never stops showing "generating" (a stale
// marker): the grace keeps restarting, but the ask still ends at its
// deadline as answer_timeout and does not hang.
test("page.stale_generating_after_an_empty_stream_ends_at_the_deadline", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let turn = null as AccountTurn | null;
  let stream = null as AnswerStream | null;
  chat.onSend = () => {
    turn = chat.beginTurn();
    const s = pageFetchesAnswerStream();
    stream = s;
    later(100, () => {
      s.event({ type: "title_generation", title: "Greeting" });
      s.done();
    });
  };
  // The failed gateway turn is abandoned with a Stop click; let it work.
  chat.stop.onClick = () => turn?.finish("cancelled");

  const started = Date.now();
  const reply = await ask("r4e", "hello", { target: { kind: "conversation", url: CHAT }, timeoutMs: 6_000 });
  const elapsed = Date.now() - started;

  assert.equal(reply.ok, false, JSON.stringify(reply));
  assert.equal(reply.errorCode, "answer_timeout");
  assert.ok(elapsed >= 5_500, `failed after ${elapsed} ms, before the deadline`);
  assert.equal(chat.stop.clicks, 1);
  stream!.close();
  await assertPageAtRest();
});

// HTTP 429 on the answer stream: the shim reports it and the ask fails as
// `rate_limited` (TASK-010 must keep this working).
test("page.http_429_fails_the_ask_as_rate_limited", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  routeFetch((url) => (url === CONVERSATION_URL ? new Response("Too many requests", { status: 429 }) : null));
  chat.onSend = () => void win.fetch(CONVERSATION_URL, { method: "POST" });

  const started = Date.now();
  const reply = await ask("r5", "hello", { target: { kind: "conversation", url: CHAT } });

  assert.equal(reply.ok, false, JSON.stringify(reply));
  assert.equal(reply.errorCode, "rate_limited");
  assert.ok(Date.now() - started < 5_000, `failed after ${Date.now() - started} ms`);
  assert.deepEqual(chat.sentPrompts, ["hello"]);
  assert.equal(chat.stop.clicks, 0);
  await assertPageAtRest();
});

// The prompt may stay in the composer after the 429 (the page keeps it for a
// retry): the answer to the click was the 429, so Send is pressed once.
test("page.http_429_with_the_prompt_left_in_the_composer_is_not_sent_again", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  routeFetch((url) => (url === CONVERSATION_URL ? new Response("Too many requests", { status: 429 }) : null));
  let clicks = 0;
  chat.send.onClick = () => {
    clicks++;
    void win.fetch(CONVERSATION_URL, { method: "POST" });
  };

  const reply = await ask("r5b", "hello", { target: { kind: "conversation", url: CHAT } });

  assert.equal(reply.ok, false, JSON.stringify(reply));
  assert.equal(reply.errorCode, "rate_limited");
  assert.equal(clicks, 1);
  assert.equal(chat.composerText, "hello");
  await assertPageAtRest();
});
