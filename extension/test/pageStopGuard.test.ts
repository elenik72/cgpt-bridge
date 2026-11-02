// The Stop guard (AUD-004, FU-PAGE): the extension presses Stop for a
// gateway ask (abandoning a failed turn, a stop naming the ask, a host stop
// without an askId) only while the page shows that ask's chat: the saved
// conversation by its path, or the new chat the ask created by its one user
// turn. The user may have switched the window to a chat of their own that is
// generating; a Stop there would cut off their answer.

import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";

import { ask, goTo, h, loadPage, sendToContent } from "./harness/fakeWindow";
import { AccountChat, later, settlePage, type AccountTurn } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const CHAT = "https://chatgpt.com/c/abc";
const TIMEOUT_MS = 20_000;

function stopFromGateway(id: string, askId?: string): Promise<Record<string, unknown>> {
  return sendToContent({ id, type: "stop", ...(askId === undefined ? {} : { askId }) });
}

// Silences console.warn for the test; returns the "Stop not pressed" lines
// logged so far (the guard says why it pressed nothing).
function notPressedWarnings(t: TestContext): () => string[] {
  const warn = t.mock.method(console, "warn", () => {});
  return () =>
    warn.mock.calls
      .map((c) => c.arguments.map(String).join(" "))
      .filter((line) => line.startsWith("[cgpt-bridge] Stop not pressed:"));
}

// The account turn's user message, as the page renders it.
function userBubble(text: string) {
  return h("div", { sels: ["[data-user-message-bubble]"], text });
}

// After Send the user opens another chat and starts an answer there, and it
// keeps generating. The ask watches its chat's path: once the page has been
// away for the grace (3 s), it ends as target_mismatch at once instead of
// holding the tab until its deadline, presses nothing, and the next ask runs.
test("stopGuard.switched_chat_still_generating_ends_the_ask_within_the_grace", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    let switchedAt = 0;
    let theirs = null as AccountTurn | null;
    chat.onSend = () => {
      chat.beginTurn();
      later(100, () => {
        chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
        theirs = chat.beginTurn();
        switchedAt = Date.now();
      });
    };
    const reply = await ask("g1", "project question", {
      target: { kind: "conversation", url: CHAT },
      timeoutMs: TIMEOUT_MS,
    });
    const after = Date.now() - switchedAt;
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.match(String(reply.message), /left the target chat while the answer was awaited: at \/c\/other/);
    assert.ok(after >= 3_000 && after < 4_500, `failed ${after} ms after the switch`);
    assert.equal(chat.stop.clicks, 0);
    assert.deepEqual(warned(), []);
    // The gateway's stop for the failed turn presses nothing there either.
    const stopped = await stopFromGateway("g1-stop");
    assert.deepEqual(stopped, { id: "g1-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
    // The tab is free: the next ask (to the chat now shown) runs and answers.
    theirs!.render("Their answer.");
    theirs!.finish();
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => {
        turn.render("Next answer.");
        turn.finish();
      });
    };
    const next = await ask("g1-next", "next question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/other" },
    });
    assert.equal(next.ok, true, JSON.stringify(next));
    assert.equal(next.text, "Next answer.");
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

// A look at another chat shorter than the grace (or ChatGPT moving the
// address for a moment) does not fail the ask.
test("stopGuard.a_short_look_at_another_chat_keeps_the_ask", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      chat.beginTurn();
      later(100, () => chat.showOtherChat("/c/other", []));
      later(2_000, () => {
        chat.showOtherChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
        const turn = chat.beginTurn();
        later(100, () => {
          turn.render("Our answer.");
          turn.finish();
        });
      });
    };
    const reply = await ask("g1b", "project question", { target: { kind: "conversation", url: CHAT } });
    assert.deepEqual(reply, { id: "g1b", ok: true, kind: "test.ask", text: "Our answer.", conversationUrl: CHAT });
  } finally {
    await settlePage();
  }
});

// The same for a new-chat ask: the user opened a saved chat of their own.
test("stopGuard.new_chat_switched_away_at_the_deadline_gets_no_stop", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/");
  try {
    chat.onSend = (prompt) => {
      goTo("/c/local-chatgpt:1e2f");
      chat.beginTurn().el.append(userBubble(prompt));
      later(100, () => {
        chat.showOtherChat("/c/mine", [{ id: "x1", text: "An answer to someone else." }]);
        chat.beginTurn().el.append(userBubble("A question the user asks now."));
      });
    };
    const reply = await ask("g2", "project question", { target: { kind: "new_chat" }, timeoutMs: TIMEOUT_MS });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.equal(chat.stop.clicks, 0);
    assert.equal(warned().length, 1);
    const stopped = await stopFromGateway("g2-stop");
    assert.deepEqual(stopped, { id: "g2-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

// Our own chat is still generating at the deadline: the turn is abandoned
// with one Stop click, as before.
test("stopGuard.own_chat_generating_at_the_deadline_is_stopped", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let turn = null as AccountTurn | null;
  try {
    chat.onSend = () => {
      turn = chat.beginTurn();
    };
    chat.stop.onClick = () => turn?.finish("cancelled");
    const reply = await ask("g3", "project question", {
      target: { kind: "conversation", url: CHAT },
      timeoutMs: TIMEOUT_MS,
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "answer_timeout");
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

// A new chat that times out while generating is abandoned too: the page
// shows the chat the ask created (its one user turn holds the prompt),
// whatever its address became.
test("stopGuard.own_new_chat_generating_at_the_deadline_is_stopped", async () => {
  const chat = new AccountChat("/");
  let turn = null as AccountTurn | null;
  try {
    chat.onSend = (prompt) => {
      goTo("/c/local-chatgpt:4a5b");
      const t = chat.beginTurn();
      t.el.append(userBubble(prompt));
      turn = t;
      later(600, () => goTo("/c/saved-6"));
    };
    chat.stop.onClick = () => turn?.finish("cancelled");
    const reply = await ask("g4", "project question", { target: { kind: "new_chat" }, timeoutMs: TIMEOUT_MS });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "answer_timeout");
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

// Our turn ended without an answer; at the deadline nothing generates, and
// the user switches during the abandon's start grace to a chat of their own
// that then generates. The check right before the click holds.
test("stopGuard.switch_during_the_abandon_grace_gets_no_stop", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    const started = Date.now();
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => turn.finish());
      later(started + TIMEOUT_MS + 300 - Date.now(), () => {
        chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
        chat.beginTurn();
      });
    };
    const reply = await ask("g5", "project question", {
      target: { kind: "conversation", url: CHAT },
      timeoutMs: TIMEOUT_MS,
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.equal(chat.stop.clicks, 0);
    assert.equal(warned().length, 1);
  } finally {
    await settlePage();
  }
});

// A stop naming the running ask, sent after the user switched to a chat
// that generates: it cancels the ask but presses nothing.
test("stopGuard.stop_for_the_running_ask_in_another_chat_presses_nothing", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      chat.beginTurn();
      later(100, () => {
        chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
        chat.beginTurn();
      });
    };
    const pending = ask("g6", "project question", {
      target: { kind: "conversation", url: CHAT },
      timeoutMs: TIMEOUT_MS,
    });
    let stopped: Record<string, unknown> | null = null;
    let stoppedAt = 0;
    later(1_000, () => {
      void stopFromGateway("g6-stop", "g6").then((r) => {
        stopped = r;
        stoppedAt = Date.now();
      });
    });
    const reply = await pending;
    assert.deepEqual(stopped, { id: "g6-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    // The refused stop ends the ask at once (not at the path check's grace).
    assert.ok(Date.now() - stoppedAt < 500, `ended ${Date.now() - stoppedAt} ms after the stop`);
    assert.equal(chat.stop.clicks, 0);
    assert.equal(warned().length, 1);
  } finally {
    await settlePage();
  }
});

// The ask answered; later the user generates in another chat in the same
// window. A host stop without an askId presses nothing there.
test("stopGuard.host_stop_after_an_answer_spares_another_chat", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => {
        turn.render("Our answer.");
        turn.finish();
      });
    };
    const reply = await ask("g7", "project question", { target: { kind: "conversation", url: CHAT } });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
    chat.beginTurn();
    const stopped = await stopFromGateway("g7-stop");
    assert.deepEqual(stopped, { id: "g7-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
    assert.equal(warned().length, 1);
    // Back in our chat, generating: the same stop presses Stop there.
    chat.showOtherChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
    const running = chat.beginTurn();
    chat.stop.onClick = () => running.finish("cancelled");
    const again = await stopFromGateway("g7-stop-2");
    assert.deepEqual(again, { id: "g7-stop-2", ok: true, kind: "stop", wasGenerating: true });
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

// No gateway ask ran (the last one had no target, as the CLI's): a host stop
// without an askId has no chat to stop and presses nothing.
test("stopGuard.host_stop_without_a_gateway_ask_presses_nothing", async (t) => {
  const warn = t.mock.method(console, "warn", () => {});
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => {
        turn.render("An answer.");
        turn.finish();
      });
    };
    const reply = await ask("g8", "cli question");
    assert.equal(reply.ok, true, JSON.stringify(reply));
    chat.beginTurn();
    const stopped = await stopFromGateway("g8-stop");
    assert.deepEqual(stopped, { id: "g8-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
    assert.deepEqual(
      warn.mock.calls.map((c) => c.arguments.join(" ")),
      ["[cgpt-bridge] Stop not pressed: no gateway ask has run in this page."],
    );
  } finally {
    await settlePage();
  }
});

// The user switched to a chat that is not generating: nothing to press, and
// the ask still ends as target_mismatch (the page left its chat).
test("stopGuard.switched_to_an_idle_chat_at_the_deadline_fails_as_mismatch", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    const started = Date.now();
    chat.onSend = () => {
      chat.beginTurn();
      // An empty chat (no answer there passes for ours), opened a second
      // before the deadline: inside the path check's grace.
      later(started + TIMEOUT_MS - 1_000 - Date.now(), () => chat.showOtherChat("/c/other", []));
    };
    const reply = await ask("g9", "project question", {
      target: { kind: "conversation", url: CHAT },
      timeoutMs: TIMEOUT_MS,
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.equal(chat.stop.clicks, 0);
    assert.equal(warned().length, 1);
  } finally {
    await settlePage();
  }
});

// Before typing, an earlier answer still runs in the target chat (its turn
// reads in_progress, no Stop button yet); while the ask waits for Stop to
// show, the user opens a chat of their own that generates. The ask presses
// nothing there and fails before Send.
test("stopGuard.settling_an_earlier_answer_spares_another_chat", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.addPastTurn({ id: "m1", text: "An answer still" }).el.setAttribute("data-talvt-turn-state", "in_progress");
    later(1_500, () => {
      chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
      chat.beginTurn();
    });
    const reply = await ask("g10", "project question", { target: { kind: "conversation", url: CHAT } });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.deepEqual(chat.sentPrompts, []);
    assert.equal(chat.stop.clicks, 0);
    assert.deepEqual(warned(), [
      "[cgpt-bridge] Stop not pressed: The page left the target chat before the prompt was sent: " +
        "at /c/other, want https://chatgpt.com/c/abc.",
    ]);
  } finally {
    await settlePage();
  }
});

// A stop for a running new-chat ask after the user opened a chat of their
// own: no path to watch, but the refused stop ends the ask at once.
test("stopGuard.refused_stop_ends_a_new_chat_ask_at_once", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/");
  try {
    chat.onSend = (prompt) => {
      goTo("/c/local-chatgpt:6c7d");
      chat.beginTurn().el.append(userBubble(prompt));
      later(100, () => {
        chat.showOtherChat("/c/mine", [{ id: "x1", text: "An answer to someone else." }]);
        chat.beginTurn().el.append(userBubble("A question the user asks now."));
      });
    };
    const started = Date.now();
    const pending = ask("g11", "project question", { target: { kind: "new_chat" }, timeoutMs: TIMEOUT_MS });
    later(1_000, () => void stopFromGateway("g11-stop", "g11"));
    const reply = await pending;
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.ok(Date.now() - started < 3_000, `ended after ${Date.now() - started} ms`);
    assert.equal(chat.stop.clicks, 0);
    assert.equal(warned().length, 1);
  } finally {
    await settlePage();
  }
});

// Markup drift: the new chat generates (a Stop button shows) but no user turn
// is recognised. That is not known to be another chat: nothing is pressed,
// the reason is logged, and the ask keeps its own error (answer_timeout).
test("stopGuard.drift_with_no_user_turn_keeps_the_original_error", async (t) => {
  const warned = notPressedWarnings(t);
  const chat = new AccountChat("/");
  try {
    chat.onSend = () => {
      goTo("/c/local-chatgpt:8e9f");
      chat.showStop();
    };
    const reply = await ask("g12", "project question", { target: { kind: "new_chat" }, timeoutMs: TIMEOUT_MS });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "answer_timeout");
    assert.equal(chat.stop.clicks, 0);
    assert.deepEqual(warned(), [
      "[cgpt-bridge] Stop not pressed: no user turn is recognised in the new chat at /c/local-chatgpt:8e9f " +
        "(page markup changed?).",
    ]);
    // The guard is not set: once the turn shows, the gateway's stop works.
    const running = chat.beginTurn();
    chat.stop.onClick = () => running.finish("cancelled");
    const stopped = await stopFromGateway("g12-stop");
    assert.deepEqual(stopped, { id: "g12-stop", ok: true, kind: "stop", wasGenerating: true });
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});
