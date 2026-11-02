// A whole gateway ask on the "account" markup as found live on 2026-10-28
// (research/2026-10-28-live-sse-and-markup.md, the old-to-new table), in the
// fake window. The page is snapshot HTML (test/harness/domSnapshot.ts), so the
// adapter's selectors are evaluated against the attributes instead of being
// looked up by name as in chatPage.ts. Synthetic, minimal, no real user text;
// how the turn states nest was not recorded, so every message is a turn of its
// own here (two turn states per exchange).

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, doc, goTo, loadPage, resetPage } from "./harness/fakeWindow";
import type { FakeElement } from "./harness/fakeWindow";
import { loadSnapshot, parseHtml } from "./harness/domSnapshot";
import { later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

const PAGE = `
<main>
  <div data-thread=""></div>
  <form>
    <div contenteditable="true" role="textbox" data-composer-markdown="" aria-label="Ask anything"></div>
    <button type="submit" aria-label="Send prompt"></button>
  </form>
</main>`;

const userTurn = (text: string): string =>
  `<section data-talvt-turn-state="complete"><div data-user-message-bubble=""><div>${text}</div></div></section>`;

const answerTurn = (id: string, state: string, paragraphs: readonly string[]): string =>
  `<section data-talvt-turn-state="${state}">` +
  `<h4 data-conversation-role="assistant">ChatGPT said:</h4>` +
  `<div data-chatgpt-selection-message-id="${id}"><div data-dil-source-message-id="${id}" data-dil-message-id="${id}">` +
  `<div data-markdown-copy="" data-markdown-text-style="">` +
  paragraphs.map((p) => `<p data-d-text-flow="">${p}</p>`).join("") +
  `</div></div></div></section>`;

function el(selector: string): FakeElement {
  const found = doc.querySelector(selector);
  if (!found) throw new Error(`not in the snapshot: ${selector}`);
  return found;
}

/**
 * The page at `path` with `past` HTML turns. A Send click takes the prompt,
 * shows it as a user turn and an answer turn that is still running, then
 * renders the answer and ends the turn (`complete`); `afterSend` runs on the
 * click (an address change, say).
 */
function showChat(path: string, past: string, answerId: string, afterSend: () => void = () => {}): string[] {
  resetPage(path);
  loadSnapshot(PAGE);
  const thread = el("[data-thread]");
  thread.append(...parseHtml(past));
  const composer = el('[role="textbox"]');
  const sent: string[] = [];
  el('button[type="submit"]').onClick = () => {
    const prompt = composer.textContent;
    if (prompt.trim() === "") return;
    sent.push(prompt);
    composer.textContent = "";
    afterSend();
    thread.append(...parseHtml(userTurn(prompt) + answerTurn(answerId, "in_progress", [])));
    const root = (): FakeElement => thread.querySelectorAll("[data-markdown-copy]").at(-1)!;
    later(100, () => root().append(...parseHtml('<p data-d-text-flow="">Four</p>')));
    later(300, () => {
      root().replaceChildren(...parseHtml('<p data-d-text-flow="">Four.</p><p data-d-text-flow="">Two plus two.</p>'));
      const turns = thread.querySelectorAll("[data-talvt-turn-state]");
      turns.at(-1)!.setAttribute("data-talvt-turn-state", "complete");
    });
  };
  return sent;
}

test("markup_2026_10_28.new_chat_ask_reads_the_answer", async () => {
  try {
    const sent = showChat("/", "", "msg-a1", () => {
      goTo("/c/local-chatgpt:5f0c");
      later(600, () => goTo("/c/saved-1"));
    });
    const reply = await ask("n1", "What is two plus two?", { target: { kind: "new_chat" } });
    assert.deepEqual(reply, {
      id: "n1",
      ok: true,
      kind: "test.ask",
      text: "Four.\nTwo plus two.",
      conversationUrl: "https://chatgpt.com/c/saved-1",
    });
    assert.deepEqual(sent, ["What is two plus two?"]);
  } finally {
    await settlePage();
  }
});

// The page as recorded on 2026-10-28 with the projects check
// (research/2026-10-28-chatgpt-projects-live.md, "Попутно"): ONE turn
// container holds the user's message and the answer; the answer block is a
// `[data-markdown-copy]` that carries `data-dil-source-message-id` itself;
// two disclaimer `[data-markdown-copy]` blocks sit outside the turn and an
// empty one inside the composer form, in that page order.
test("markup_2026_10_28.one_turn_with_disclaimer_and_composer_blocks_reads_the_answer", async () => {
  try {
    resetPage("/");
    loadSnapshot(`
<main>
  <div data-thread=""></div>
  <div data-markdown-copy=""><p>ChatGPT can make mistakes.</p></div>
  <div data-markdown-copy=""><p>ChatGPT can make mistakes.</p></div>
  <form>
    <div data-markdown-copy=""></div>
    <div contenteditable="true" role="textbox" data-composer-markdown="" aria-label="Ask anything"></div>
    <button type="submit" aria-label="Send prompt"></button>
  </form>
</main>`);
    const thread = el("[data-thread]");
    const composer = el('[role="textbox"]');
    const sent: string[] = [];
    el('button[type="submit"]').onClick = () => {
      const prompt = composer.textContent;
      if (prompt.trim() === "") return;
      sent.push(prompt);
      composer.textContent = "";
      goTo("/c/saved-2");
      thread.append(
        ...parseHtml(
          `<section data-talvt-turn-state="in_progress">` +
            `<div data-user-message-bubble=""><div>${prompt}</div></div>` +
            `<div data-markdown-copy="" data-dil-source-message-id="msg-b1"></div>` +
            `</section>`,
        ),
      );
      const answer = (): FakeElement => el("[data-dil-source-message-id]");
      later(100, () => answer().append(...parseHtml('<p data-d-text-flow="">Four</p>')));
      later(300, () => {
        answer().replaceChildren(...parseHtml('<p data-d-text-flow="">Four.</p>'));
        el("[data-talvt-turn-state]").setAttribute("data-talvt-turn-state", "complete");
      });
    };
    const reply = await ask("n2", "What is two plus two?", { target: { kind: "new_chat" } });
    assert.deepEqual(reply, {
      id: "n2",
      ok: true,
      kind: "test.ask",
      text: "Four.",
      conversationUrl: "https://chatgpt.com/c/saved-2",
    });
    assert.deepEqual(sent, ["What is two plus two?"]);
  } finally {
    await settlePage();
  }
});

test("markup_2026_10_28.conversation_ask_reads_the_new_answer_only", async () => {
  try {
    const past = userTurn("An earlier question") + answerTurn("msg-p1", "complete", ["An earlier answer."]);
    const sent = showChat("/c/abc", past, "msg-a2");
    const reply = await ask("c1", "What is two plus two?", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.deepEqual(reply, {
      id: "c1",
      ok: true,
      kind: "test.ask",
      text: "Four.\nTwo plus two.",
      conversationUrl: "https://chatgpt.com/c/abc",
    });
    assert.deepEqual(sent, ["What is two plus two?"]);
  } finally {
    await settlePage();
  }
});

// L2 on this markup: the answer stream is cut mid-answer, the page shows the
// partial text with a turn state the adapter does not know, then recovers
// the whole answer and ends the turn.
test("markup_2026_10_28.stream_cut_then_page_recovers_the_answer", async () => {
  let stream = null as AnswerStream | null;
  try {
    const past = userTurn("An earlier question") + answerTurn("msg-p1", "complete", ["An earlier answer."]);
    showChat("/c/abc", past, "msg-a3");
    const thread = el("[data-thread]");
    const composer = el('[role="textbox"]');
    el('button[type="submit"]').onClick = () => {
      const prompt = composer.textContent;
      composer.textContent = "";
      const s = pageFetchesAnswerStream();
      stream = s;
      thread.append(...parseHtml(userTurn(prompt) + answerTurn("msg-a3", "in_progress", ["Four"])));
      const turn = (): FakeElement => thread.querySelectorAll("[data-talvt-turn-state]").at(-1)!;
      const root = (): FakeElement => thread.querySelectorAll("[data-markdown-copy]").at(-1)!;
      later(50, () => s.startAnswer("m-a3", "Four"));
      later(200, () => {
        s.cut();
        turn().setAttribute("data-talvt-turn-state", "interrupted");
      });
      later(5_200, () => {
        root().replaceChildren(...parseHtml('<p data-d-text-flow="">Four.</p><p data-d-text-flow="">Two plus two.</p>'));
        turn().setAttribute("data-talvt-turn-state", "complete");
      });
    };
    const reply = await ask("c2", "What is two plus two?", {
      target: { kind: "conversation", url: "https://chatgpt.com/c/abc" },
    });
    assert.equal(reply.text, "Four.\nTwo plus two.", JSON.stringify(reply));
  } finally {
    await settlePage([stream]);
  }
});
