// Shim reports that arrive while submit() still waits for a sign that the
// prompt went out (B8, TASK-015 review #6) on the fake window: they belong
// to the answer wait that starts right after.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage, sendToContent, shimReports } from "./harness/fakeWindow";
import { AccountChat, later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

// The page clears the composer late, so the click is confirmed only by the
// stream's start, at submit's next poll; by then the empty stream has
// already ended. Its sse-done still ends the ask ≈ 3 s after the stream
// (TASK-015's empty-stream grace), not at the 20 s deadline.
test("page.empty_stream_that_ends_before_the_send_is_confirmed_fails_fast", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  try {
    chat.send.onClick = () => {
      chat.sentPrompts.push(chat.composerText);
      const s = pageFetchesAnswerStream();
      stream = s;
      s.event({ type: "title_generation", title: "Greeting" });
      s.done();
      later(1_000, () => {
        chat.composer.textContent = "";
      });
    };
    const reply = await ask("b8", "hello", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
      timeoutMs: 20_000,
    });
    const afterStream = Date.now() - stream!.endedAt;
    assert.deepEqual(chat.sentPrompts, ["hello"]);
    assert.deepEqual(
      shimReports().map((r) => [r.kind, r.text]),
      [
        ["sse-start", ""],
        ["sse-done", ""],
      ],
    );
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "answer_not_detected", JSON.stringify(reply));
    assert.ok(afterStream >= 2_000 && afterStream <= 4_500, `failed ${afterStream} ms after the stream ended`);
  } finally {
    await settlePage([stream]);
  }
});

// A host Stop for the ask is refused while the click is still being
// confirmed (drifted markup: the new chat shows "generating" but no user
// turn), and the whole answer's stream ended in the same window. The refusal
// is the ask's outcome ("cancelled by stop"), as it was before early reports
// were kept: the wait's first check comes before the early reports.
test("page.stop_refused_while_the_send_is_confirmed_wins_over_an_early_answer", async (t) => {
  t.mock.method(console, "warn", () => {});
  const chat = new AccountChat("/");
  let stream = null as AnswerStream | null;
  let stopped: Record<string, unknown> | null = null;
  try {
    // "Generating" already shows once the prompt is typed, so only the
    // stream's start confirms the click, at submit's next poll.
    chat.composer.addEventListener("input", () => chat.form.append(chat.stop));
    chat.send.onClick = () => {
      chat.sentPrompts.push(chat.composerText);
      const s = pageFetchesAnswerStream();
      stream = s;
      s.startAnswer("m1", "The whole answer.");
      s.endTurn();
      s.done();
      later(20, () => {
        void sendToContent({ id: "b8s-stop", type: "stop", askId: "b8s" }).then((r) => {
          stopped = r;
        });
      });
    };
    const reply = await ask("b8s", "project question", { target: { kind: "new_chat" }, timeoutMs: 20_000 });
    assert.deepEqual(chat.sentPrompts, ["project question"]);
    assert.deepEqual(
      shimReports().map((r) => [r.kind, r.text]),
      [
        ["sse-start", ""],
        ["sse-tokens", "The whole answer."],
        ["sse-done", "The whole answer."],
      ],
    );
    assert.deepEqual(stopped, { id: "b8s-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "cancelled", JSON.stringify(reply));
    assert.equal(chat.stop.clicks, 0);
  } finally {
    chat.stop.remove();
    await settlePage([stream]);
  }
});
