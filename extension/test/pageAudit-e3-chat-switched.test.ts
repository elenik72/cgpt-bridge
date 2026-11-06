// Audit probe E3 (AUD-004, research/CODE_AUDIT.md) on the fake window, and
// the rest of AUD-004: after the answer a targeted ask checks that it came
// from the target chat and was not cut off by a Stop.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, goTo, h, loadPage, sendToContent } from "./harness/fakeWindow";
import { AccountChat, later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { assertAnsweredInTarget, showsSameChatId } from "../src/chatgptAdapter";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

// The gateway's follow-up to a failed turn: a stop without an askId.
function stopFromGateway(id: string): Promise<Record<string, unknown>> {
  return sendToContent({ id, type: "stop" });
}

// The account turn's user message, as the page renders it.
function userBubble(text: string) {
  return h("div", { sels: ["[data-user-message-bubble]"], text });
}

// After the Send click the gateway's window is switched to another chat (a
// sidebar click). The ask used to return that chat's last answer and
// address, and the session was rebound to it.
test("audit.e3_chat_switched_after_send_fails_the_turn", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      chat.beginTurn();
      later(100, () =>
        chat.showOtherChat("/c/other", [
          { id: "o1", text: "first answer there" },
          { id: "o2", text: "second answer there" },
          { id: "o3", text: "ANSWER FROM ANOTHER CHAT" },
        ]),
      );
    };
    const reply = await ask("e3", "project question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(reply.ok, false, `answered from another chat: ${JSON.stringify(reply)}`);
    assert.equal(reply.errorCode, "target_mismatch");
    // The user goes on in the other chat; the gateway's stop for the failed
    // turn must not press Stop there.
    chat.beginTurn();
    const stopped = await stopFromGateway("e3-stop");
    assert.deepEqual(stopped, { id: "e3-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

// The bridge's own stream completes (shim) while the chat the user switched
// to is generating: the turn fails, and the stop that follows presses nothing.
test("audit.e3_stop_after_a_mismatch_leaves_the_other_chat_generating", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  try {
    chat.onSend = () => {
      chat.beginTurn();
      const s = pageFetchesAnswerStream();
      stream = s;
      later(50, () => s.startAnswer("m1", "Our answer"));
      later(100, () => {
        chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
        chat.beginTurn();
      });
      later(200, () => {
        s.append(" in full.");
        s.endTurn();
        s.done();
      });
    };
    const reply = await ask("e3-gen", "project question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.equal(chat.stop.clicks, 0);
    const stopped = await stopFromGateway("e3-gen-stop");
    assert.deepEqual(stopped, { id: "e3-gen-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage([stream]);
  }
});

// The bridge's stream ends without answer text after the page was switched
// to another (empty) chat: the fast failure of TASK-015 says
// target_mismatch, not answer_not_detected, and the gateway's stop that
// follows presses nothing in the chat the user now generates in.
test("audit.e3_empty_stream_after_a_chat_switch_fails_as_mismatch", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  let stream = null as AnswerStream | null;
  try {
    chat.onSend = () => {
      chat.beginTurn();
      const s = pageFetchesAnswerStream();
      stream = s;
      later(100, () => {
        s.event({ type: "title_generation", title: "Greeting" });
        s.done();
      });
      later(200, () => chat.showOtherChat("/c/other", []));
    };
    const started = Date.now();
    const reply = await ask("e3-empty", "project question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.ok(Date.now() - started < 6_000, `failed after ${Date.now() - started} ms`);
    assert.equal(chat.stop.clicks, 0);
    // The user starts an answer in the other chat; Stop is shown there.
    chat.beginTurn();
    const stopped = await stopFromGateway("e3-empty-stop");
    assert.deepEqual(stopped, { id: "e3-empty-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage([stream]);
  }
});

// A new-chat ask on a page whose markup drifted: no reply is recognised for
// 30 s and the user turn is not recognised either (0 turns). That is a DOM
// failure, not a switched chat: answer_not_detected, and the stop guard is
// not set, so the gateway's stop still presses Stop.
test("audit.e3_new_chat_no_signal_with_dom_drift_stays_answer_not_detected", async () => {
  const chat = new AccountChat("/");
  try {
    chat.onSend = () => {
      goTo("/c/local-chatgpt:7a1d");
      chat.thread.append(h("div", { text: "An answer in markup the adapter does not know." }));
    };
    const reply = await ask("e3-drift", "project question", {
      target: { kind: "new_chat" },
      timeoutMs: 40_000,
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "answer_not_detected");
    const running = chat.beginTurn();
    chat.stop.onClick = () => running.finish("cancelled");
    const stopped = await stopFromGateway("e3-drift-stop");
    assert.deepEqual(stopped, { id: "e3-drift-stop", ok: true, kind: "stop", wasGenerating: true });
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

// The guard is one-shot: once a new ask has run, a stop works as before.
test("audit.e3_stop_guard_ends_with_the_next_ask", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      chat.beginTurn();
      later(100, () => chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]));
    };
    const failed = await ask("e3-once", "project question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(failed.errorCode, "target_mismatch");
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => {
        turn.render("Answer in the other chat.");
        turn.finish();
      });
    };
    const next = await ask("e3-next", "next question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/other" },
    });
    assert.equal(next.ok, true, JSON.stringify(next));
    const running = chat.beginTurn();
    chat.stop.onClick = () => running.finish("cancelled");
    const stopped = await stopFromGateway("e3-next-stop");
    assert.deepEqual(stopped, { id: "e3-next-stop", ok: true, kind: "stop", wasGenerating: true });
    assert.equal(chat.stop.clicks, 1);
  } finally {
    await settlePage();
  }
});

// A new chat holds exactly the gateway's prompt. Here a second message went
// into it (typed in the gateway's window) and the answer is to that one.
test("audit.e3_new_chat_with_two_user_turns_fails_the_turn", async () => {
  const chat = new AccountChat("/");
  try {
    chat.onSend = () => {
      goTo("/c/local-chatgpt:5f0c");
      const own = chat.beginTurn();
      later(100, () => {
        own.render("Answer to the gateway's prompt.");
        own.finish();
        const other = chat.beginTurn();
        later(100, () => {
          other.render("Answer to another message.");
          other.finish();
        });
      });
      later(600, () => goTo("/c/saved-2"));
    };
    const reply = await ask("e3-new", "project question", { target: { kind: "new_chat" } });
    assert.equal(reply.ok, false, `answered with two user turns: ${JSON.stringify(reply)}`);
    assert.equal(reply.errorCode, "target_mismatch");
  } finally {
    await settlePage();
  }
});

// Seen live (block 6, 02.11): the new chat got its address, then the user
// clicked another chat (one with two questions) in the bridge window while
// the answer was written. The error used to count that chat's user turns
// ("holds 2 user turns… want 1"); it now says what happened, and that the
// answer goes on in the new chat. The stop that follows presses nothing.
test("audit.e3_new_chat_left_by_a_click_says_so", async () => {
  const chat = new AccountChat("/");
  let stream = null as AnswerStream | null;
  try {
    chat.onSend = () => {
      goTo("/c/local-chatgpt:1e2f");
      chat.beginTurn();
      const s = pageFetchesAnswerStream();
      stream = s;
      later(50, () => s.startAnswer("m1", "Our answer"));
      later(100, () => goTo("/c/saved-new"));
      later(300, () => {
        chat.showOtherChat("/c/cities", [
          { id: "c1", text: "Ten cities." },
          { id: "c2", text: "A hundred cities." },
        ]);
        chat.beginTurn();
      });
      later(400, () => {
        s.append(" in full.");
        s.endTurn();
        s.done();
      });
    };
    const reply = await ask("e3-click", "project question", { target: { kind: "new_chat" } });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.match(reply.message ?? "", /switched to another chat by a click/);
    assert.match(reply.message ?? "", /shows \/c\/cities, the prompt went to https:\/\/chatgpt\.com\/c\/saved-new/);
    assert.match(reply.message ?? "", /goes on answering it in that chat/);
    assert.doesNotMatch(reply.message ?? "", /user turns/);
    const stopped = await stopFromGateway("e3-click-stop");
    assert.deepEqual(stopped, { id: "e3-click-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage([stream]);
  }
});

// A new chat in a project may first show `/c/<id>` and only later its
// project prefix (page test: pageProjects). Compared by chat id only.
const PROJECT = "g-p-0123456789abcdef0123456789abcdef";
test("audit.e3_chat_id_compare_ignores_the_project_prefix_only", () => {
  const url = "https://chatgpt.com/c/saved-p";
  assert.equal(showsSameChatId(url, "/c/saved-p"), true);
  assert.equal(showsSameChatId(url, `/g/${PROJECT}-my-proj/c/saved-p`), true);
  assert.equal(showsSameChatId(url, "/c/other"), false);
  assert.equal(showsSameChatId(url, `/g/${PROJECT}-my-proj/c/other`), false);
  assert.equal(showsSameChatId(url, `/g/${PROJECT}/project`), false);
  assert.equal(showsSameChatId(url, "/"), false);
});

// Stop pressed in the gateway's window: the turn ends `cancelled` and its
// cut-off answer must not pass for a whole one.
test("audit.e3_turn_stopped_in_the_page_fails_as_cancelled", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    chat.onSend = () => {
      const turn = chat.beginTurn();
      later(100, () => turn.render("The first half of"));
      later(300, () => turn.finish("cancelled"));
    };
    const reply = await ask("e3-stop", "project question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(reply.ok, false, `a cut-off answer passed: ${JSON.stringify(reply)}`);
    assert.equal(reply.errorCode, "cancelled");
  } finally {
    await settlePage();
  }
});

// A new-chat ask switched to an existing chat that also has one user turn:
// its first message is not our prompt.
test("audit.e3_new_chat_switched_to_a_one_turn_chat_fails_the_turn", async () => {
  const chat = new AccountChat("/");
  try {
    chat.onSend = (prompt) => {
      goTo("/c/local-chatgpt:9b2e");
      chat.beginTurn().el.append(userBubble(prompt));
      later(100, () => {
        chat.showOtherChat("/c/one", []);
        const there = chat.addPastTurn({ id: "x1", text: "An answer to someone else." });
        // The user's message comes before its answer, as the page renders it.
        there.el.replaceChildren(userBubble("A question the user asked earlier."), ...there.el.childNodes);
      });
    };
    const reply = await ask("e3-one", "project question", { target: { kind: "new_chat" } });
    assert.equal(reply.ok, false, `answered from another chat: ${JSON.stringify(reply)}`);
    assert.equal(reply.errorCode, "target_mismatch");
  } finally {
    await settlePage();
  }
});

// Our own new chat: the bubble holds the prompt (lines and long text
// collapsed the same way on both sides). The bubble also carries a user-role
// mark: it is still one user turn.
test("audit.e3_own_new_chat_with_the_prompt_in_its_bubble_answers", async () => {
  const prompt =
    "project question\n\nwith a second paragraph that makes this prompt longer than eighty characters\nin all";
  const chat = new AccountChat("/");
  try {
    chat.onSend = (sent) => {
      goTo("/c/local-chatgpt:7a1d");
      const turn = chat.beginTurn();
      // Rendered with its own line breaks and spacing.
      const bubble = userBubble(sent.replace(/\n+/g, "\n  "));
      bubble.sels.add('[data-message-author-role="user"]');
      turn.el.append(bubble);
      later(100, () => {
        turn.render("The answer.");
        turn.finish();
      });
      later(600, () => goTo("/c/saved-3"));
    };
    const reply = await ask("e3-ok", prompt, { target: { kind: "new_chat" } });
    assert.deepEqual(reply, {
      id: "e3-ok",
      ok: true,
      kind: "test.ask",
      text: "The answer.",
      conversationUrl: "https://chatgpt.com/c/saved-3",
    });
  } finally {
    await settlePage();
  }
});

// A layout without the bubble: the text check is skipped, the turn count
// alone decides.
test("audit.e3_new_chat_without_a_bubble_answers", async () => {
  const chat = new AccountChat("/");
  try {
    chat.onSend = () => {
      goTo("/c/local-chatgpt:3c4d");
      const turn = chat.beginTurn();
      later(100, () => {
        turn.render("The answer.");
        turn.finish();
      });
      later(600, () => goTo("/c/saved-4"));
    };
    const reply = await ask("e3-nobubble", "project question", { target: { kind: "new_chat" } });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.conversationUrl, "https://chatgpt.com/c/saved-4");
  } finally {
    await settlePage();
  }
});

// The page switched to another chat while the prompt was being typed (and
// that chat is generating): the ask fails before Send, and the gateway's
// stop for it presses nothing there either.
test("audit.e3_stop_after_a_mismatch_before_send_presses_nothing", async () => {
  const chat = new AccountChat("/c/abc", [{ id: "m0", text: "An earlier answer." }]);
  try {
    let switched = false;
    chat.composer.addEventListener("input", () => {
      if (switched) return;
      switched = true;
      chat.showOtherChat("/c/other", [{ id: "o1", text: "An answer there." }]);
      chat.beginTurn();
    });
    const reply = await ask("e3-pre", "project question", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.equal(reply.errorCode, "target_mismatch");
    assert.deepEqual(chat.sentPrompts, []);
    const stopped = await stopFromGateway("e3-pre-stop");
    assert.deepEqual(stopped, { id: "e3-pre-stop", ok: true, kind: "stop", wasGenerating: false });
    assert.equal(chat.stop.clicks, 0);
  } finally {
    await settlePage();
  }
});

// How the page may render the prompt in the new chat's user message. None
// of these differences may fail the turn.
const RENDERINGS: ReadonlyArray<[string, string, string]> = [
  ["code fence re-rendered as a code block", "Run this:\n```bash\nls -la /tmp\n```\nWhat does it print?", "Run this:\nbash\nls -la /tmp\nWhat does it print?"],
  ["markdown emphasis rendered", "**Important:** keep the _tests_ green and `npm test` quiet", "Important: keep the tests green and npm test quiet"],
  ["smart quotes and dashes", 'He said "don\'t touch it" -- and left', "He said \u201cdon\u2019t touch it\u201d \u2014 and left"],
  ["a label before the message", "project question about the gateway rules", "You said:\nproject question about the gateway rules"],
  ["a zero-width space", "project question about the gateway rules", "project ques\u200btion about the gate\u200bway rules"],
  ["NFD prompt, NFC bubble", "Caf\u00e9 na\u00efve r\u00e9sum\u00e9".normalize("NFD"), "Caf\u00e9 na\u00efve r\u00e9sum\u00e9"],
  ["the gateway's own head", "\u041f\u0440\u0430\u0432\u0438\u043b\u0430 \u0448\u043b\u044e\u0437\u0430\n\n\u0422\u044b \u0440\u0430\u0431\u043e\u0442\u0430\u0435\u0448\u044c \u043a\u0430\u043a \u043c\u043e\u0434\u0435\u043b\u044c \u0430\u0433\u0435\u043d\u0442\u0430 \u2026", "\u041f\u0440\u0430\u0432\u0438\u043b\u0430 \u0448\u043b\u044e\u0437\u0430 \u0422\u044b \u0440\u0430\u0431\u043e\u0442\u0430\u0435\u0448\u044c \u043a\u0430\u043a \u043c\u043e\u0434\u0435\u043b\u044c \u0430\u0433\u0435\u043d\u0442\u0430 \u2026"],
];

// One new chat holding one user turn whose message reads `shown`.
function newChatShowing(shown: string): void {
  const chat = new AccountChat("/c/saved-5");
  chat.addPastTurn({ id: "u1", text: "An answer." }).el.append(userBubble(shown));
}

test("audit.e3_new_chat_prompt_check_tolerates_renderings", () => {
  for (const [what, prompt, shown] of RENDERINGS) {
    newChatShowing(shown);
    assert.doesNotThrow(() => assertAnsweredInTarget({ kind: "new_chat" }, prompt), what);
  }
});

test("audit.e3_new_chat_prompt_check_catches_another_message", () => {
  newChatShowing("You said:\nA question the user asked earlier, in a chat of their own.");
  assert.throws(
    () => assertAnsweredInTarget({ kind: "new_chat" }, "project question about the gateway rules"),
    (err: Error) => err.name === "TargetMismatchError",
  );
});
