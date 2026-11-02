// Audit probe E5 (AUD-003, research/CODE_AUDIT.md) on the fake window,
// fixed by TASK-011.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage } from "./harness/fakeWindow";
import { AccountChat, later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

// A CLI ask into a tab that is still generating the previous answer (left
// running by a CLI ask that returned a partial answer at its deadline).
// Before TASK-011 the prompt was typed into it and the ask waited on an
// answer that was not its own; now it is refused before anything is typed.
test(
  "audit.e5_cli_ask_into_a_generating_tab_is_refused",
  async () => {
    const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
    let previous = null as AnswerStream | null;
    try {
      chat.onSend = () => {
        const turn = chat.beginTurn();
        const p = pageFetchesAnswerStream();
        previous = p;
        later(50, () => {
          p.startAnswer("p1", "PREVIOUS ANSWER");
          turn.render("PREVIOUS ANSWER");
        });
        // The answer ends once the user (or anyone) presses Enter again.
        chat.composer.addEventListener("keydown", () =>
          later(200, () => {
            p.append(" (finished)");
            p.endTurn();
            turn.render("PREVIOUS ANSWER (finished)");
            turn.finish();
            p.done();
          }),
        );
      };
      const first = await ask("c1", "FIRST QUESTION", { timeoutMs: 3_000 });
      assert.equal(first.text, "PREVIOUS ANSWER", "precondition: the first ask returned a partial answer");
      assert.equal(chat.stop.isConnected, true, "precondition: the tab is still generating");

      // Short: the refusal comes before typing; before TASK-011 the ask ran
      // to this deadline (the previous answer's stream is not its own).
      const second = await ask("c2", "SECOND QUESTION", { timeoutMs: 5_000 });
      assert.equal(second.ok, false, `the second ask was answered: ${JSON.stringify(second)}`);
      assert.equal(second.errorCode, "previous_answer_running");
      // The CLI has no stop command: the user is pointed at the tab.
      assert.match(second.message ?? "", /press Stop in the ChatGPT tab/);
      assert.equal(chat.composerText, "", "nothing is typed into a generating tab");
      assert.deepEqual(chat.sentPrompts, ["FIRST QUESTION"]);
      // The user's tab: the CLI never presses Stop in it.
      assert.equal(chat.stop.clicks, 0, "Stop was pressed in the user's tab");
    } finally {
      await settlePage([previous]);
    }
  },
);

// An answer that has just ended: the turn still says `in_progress` for a
// moment, with Send (not Stop) already shown. The ask waits that out
// instead of refusing the tab as busy.
test("audit.e5_cli_ask_waits_out_an_answer_just_ending", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  const ending = chat.beginTurn();
  ending.render("A just finished answer.");
  chat.showSend();
  later(1_000, () => ending.finish());
  chat.onSend = () => {
    const turn = chat.beginTurn();
    later(100, () => {
      turn.render("The new answer.");
      turn.finish();
    });
  };
  try {
    const reply = await ask("c3", "QUESTION", { timeoutMs: 15_000 });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "The new answer.");
    assert.deepEqual(chat.sentPrompts, ["QUESTION"]);
  } finally {
    await settlePage();
  }
});

// Seen live: right after an answer is in, the turn stays `in_progress` with
// Stop still shown for some seconds. A CLI ask (an agent's repair prompt
// comes at once) waits that out instead of refusing the tab as busy.
test("audit.e5_cli_ask_waits_out_an_answer_ending_with_stop_shown", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  const ending = chat.beginTurn();
  ending.render("A just finished answer.");
  later(4_000, () => {
    ending.finish();
    chat.showSend();
  });
  chat.onSend = () => {
    const turn = chat.beginTurn();
    later(100, () => {
      turn.render("The new answer.");
      turn.finish();
    });
  };
  try {
    const started = Date.now();
    const reply = await ask("c4", "QUESTION", { timeoutMs: 120_000 });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "The new answer.");
    assert.ok(Date.now() - started >= 4_000, "typed before the previous turn ended");
    assert.deepEqual(chat.sentPrompts, ["QUESTION"]);
    assert.equal(chat.stop.clicks, 0, "Stop was pressed in the user's tab");
  } finally {
    await settlePage();
  }
});
