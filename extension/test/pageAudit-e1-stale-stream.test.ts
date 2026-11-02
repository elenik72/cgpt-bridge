// Audit probe P5/E1 (AUD-003, research/CODE_AUDIT.md) on the fake window.
// Like every pageAudit-* file it asserts the CORRECT behaviour and is a
// `todo` until the named task lands: it fails today without failing the
// suite, and the task that fixes it drops the `todo`. One probe per file:
// each gets a fresh bundle, window and process (see harness/fakeWindow.ts).

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage } from "./harness/fakeWindow";
import { AccountChat, later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const IN_CHAT = { kind: "conversation", url: "https://chatgpt.com/c/abc" } as const;

// A stream started while the shim was on for ask A outlives A (A ended on
// the DOM after the 3 s grace). Its `sse-done` must not be taken by the next
// ask in the same tab: it carries A's shim epoch (TASK-010).
test(
  "audit.p5_e1_stale_stream_of_the_previous_ask_does_not_answer_the_next",
  async () => {
    const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
    let streamA = null as AnswerStream | null;
    let streamB = null as AnswerStream | null;
    try {
      chat.onSend = () => {
        const turn = chat.beginTurn();
        const a = pageFetchesAnswerStream();
        streamA = a;
        later(50, () => {
          a.startAnswer("a1", "OLD ANSWER (to prompt 1)");
          turn.render("OLD ANSWER (to prompt 1)");
          // The DOM settles; the stream stays open (metadata, slow tail).
          turn.finish();
        });
      };
      const replyA = await ask("a", "prompt 1", { target: IN_CHAT });
      assert.equal(replyA.text, "OLD ANSWER (to prompt 1)", "precondition: A ended on the DOM text");
      assert.equal(streamA!.open, true, "precondition: A's stream outlived ask A");

      chat.onSend = () => {
        const turn = chat.beginTurn();
        // Stream A ends right after B's prompt went out…
        later(50, () => {
          streamA!.append(" ... rest");
          streamA!.endTurn();
          streamA!.done();
        });
        // …and the page answers B on its own stream.
        later(300, () => {
          const b = pageFetchesAnswerStream();
          streamB = b;
          later(50, () => {
            b.startAnswer("b1", "ANSWER B");
            b.endTurn();
            turn.render("ANSWER B");
            turn.finish();
            b.done();
          });
        });
      };
      const replyB = await ask("b", "prompt 2", { target: IN_CHAT });
      assert.equal(replyB.ok, true, JSON.stringify(replyB));
      assert.equal(replyB.text, "ANSWER B", "ask B answered with another ask's stream");
    } finally {
      await settlePage([streamA, streamB]);
    }
  },
);
