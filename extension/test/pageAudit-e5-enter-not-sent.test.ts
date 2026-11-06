// TASK-011's Enter fallback (AUD-003) on the fake window: a consumed Enter
// counts as sent only with a sign that the prompt went out.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage } from "./harness/fakeWindow";
import { AccountChat, later, settlePage } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

// No Send button, and the page consumes Enter without sending (the prompt
// stays in the composer). Before TASK-011 `submit()` counted a consumed Enter
// as sent and the ask waited for an answer that never came, up to its deadline.
test(
  "audit.e5_enter_fallback_without_a_send_sign_is_an_error",
  async () => {
    const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
    chat.send.remove();
    try {
      const reply = await ask("n1", "QUESTION", { timeoutMs: 12_000 });
      assert.equal(chat.composerText, "QUESTION", "precondition: the prompt stayed in the composer");
      assert.deepEqual(chat.sentPrompts, [], "precondition: the page sent nothing");
      assert.equal(reply.ok, false, JSON.stringify(reply));
      // A send failure (send_button_not_found is the existing code for it),
      // not a wait that ran to the deadline. The prompt may have gone out
      // unseen: the message says so, so the user checks before retrying.
      assert.equal(reply.errorCode, "send_button_not_found", reply.message);
      assert.match(reply.message ?? "", /may still have been sent/);
    } finally {
      await settlePage();
    }
  },
);

// The same with a short ask timeout (`--timeout-ms 11000`): the wait for a
// send sign ends before the ask's deadline, so the caller gets this error
// rather than the host giving up on a silent extension (deadline + 5 s).
test("audit.e5_enter_fallback_without_a_send_sign_errs_before_a_short_deadline", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.send.remove();
  try {
    const started = Date.now();
    const reply = await ask("n4", "QUESTION", { timeoutMs: 11_000 });
    const took = Date.now() - started;
    assert.deepEqual(chat.sentPrompts, [], "precondition: the page sent nothing");
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "send_button_not_found", reply.message);
    assert.match(reply.message ?? "", /may still have been sent/);
    assert.ok(took < 11_000, `the error came ${took} ms after the ask, past its deadline`);
  } finally {
    await settlePage();
  }
});

// No Send button; the page consumes Enter and sends 4.5 s later (it may
// clear the composer late, see resendWhileStillInComposer). The fallback
// waits as long as a click does, so the late sign counts.
test("audit.e5_enter_fallback_with_a_late_send_sign_answers", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.send.remove();
  chat.composer.addEventListener("keydown", (ev) => {
    if ((ev as KeyboardEvent).key !== "Enter") return;
    later(4_500, () => {
      const prompt = chat.composerText;
      chat.sentPrompts.push(prompt);
      chat.composer.textContent = "";
      chat.onSend(prompt);
    });
  });
  chat.onSend = () => {
    const turn = chat.beginTurn();
    later(100, () => {
      turn.render("Answer sent by Enter.");
      turn.finish();
    });
  };
  try {
    const reply = await ask("n2", "QUESTION", { timeoutMs: 30_000 });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "Answer sent by Enter.");
    assert.deepEqual(chat.sentPrompts, ["QUESTION"]);
  } finally {
    await settlePage();
  }
});

// The race of AUD-003: a generation starts in the tab while the adapter
// waits for a Send button (Stop is shown in its place), and the page
// consumes Enter without sending. A page that was already generating when
// Enter was pressed is no sign that the prompt went out.
test("audit.e5_enter_fallback_into_a_running_generation_is_an_error", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.send.remove();
  let running = null as ReturnType<AccountChat["beginTurn"]> | null;
  later(1_000, () => {
    running = chat.beginTurn();
    running.render("Someone else's answer.");
  });
  try {
    const reply = await ask("n3", "QUESTION", { timeoutMs: 20_000 });
    assert.equal(chat.stop.isConnected, true, "precondition: the tab was generating");
    assert.deepEqual(chat.sentPrompts, [], "precondition: the page sent nothing");
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "send_button_not_found", reply.message);
    assert.equal(chat.stop.clicks, 0, "Stop was pressed in the user's tab");
  } finally {
    running?.finish();
    await settlePage();
  }
});

// Live 02.11 (Windows): Send showed only some seconds after the prompt was
// typed (a window the browser slowed down), past the old 5 s wait. The ask
// waits for it (up to 20 s, within the time it needs to send) and clicks it.
test("audit.e5_send_that_shows_late_is_still_clicked", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  chat.send.remove();
  later(12_000, () => chat.showSend());
  chat.onSend = () => {
    const turn = chat.beginTurn();
    later(100, () => {
      turn.render("The answer.");
      turn.finish();
    });
  };
  try {
    const reply = await ask("late1", "QUESTION", { timeoutMs: 120_000 });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "The answer.");
    assert.deepEqual(chat.sentPrompts, ["QUESTION"]);
  } finally {
    await settlePage();
  }
});
