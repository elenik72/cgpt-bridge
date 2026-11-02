// Audit probe E2 (AUD-007, research/CODE_AUDIT.md) on the fake window: a
// stream that ended without answer text fails fast (TASK-015).

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage } from "./harness/fakeWindow";
import { AccountChat, later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

// The answer stream ends without an assistant message (only
// `title_generation` and `[DONE]`) and the DOM shows no answer. TASK-015:
// fail ≈ 3 s (± 1 s) after the stream ended, not at the deadline and not
// sooner (DOM text may still come in that grace; the guards for that are
// page.dom_answer_wins_after_an_empty_stream and its neighbours).
test(
  "audit.e2_stream_ended_without_answer_fails_fast",
  async () => {
    const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
    let stream = null as AnswerStream | null;
    try {
      chat.onSend = () => {
        const turn = chat.beginTurn();
        const s = pageFetchesAnswerStream();
        stream = s;
        later(100, () => {
          s.event({ type: "title_generation", title: "Greeting" });
          s.done();
          turn.finish();
        });
      };
      const reply = await ask("e2", "hello", {
        target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
        timeoutMs: 6_000,
      });
      const afterStream = Date.now() - stream!.endedAt;
      assert.equal(reply.ok, false, JSON.stringify(reply));
      assert.equal(reply.errorCode, "answer_not_detected");
      assert.ok(afterStream >= 2_000 && afterStream <= 4_000, `failed ${afterStream} ms after the stream ended`);
    } finally {
      await settlePage([stream]);
    }
  },
);
