// What the ChatGPT page itself does, on top of fakeWindow.ts: the
// "account" (signed-in) and "octane" (signed-out) markups as the adapter's
// inventory describes them (chatgptAdapter.ts, top), Send/Stop, turns, and
// the answer stream the page fetches from the conversation endpoint —
// through `window.fetch`, so the pageShim's wrapper sees it.
//
// Selector strings are spelled exactly as in chatgptAdapter.ts; an element
// is found only by the ones listed here.

import {
  FakeElement,
  FakeTextArea,
  doc,
  goTo,
  h,
  resetPage,
  routeFetch,
  shimActive,
  until,
  win,
  windowListenerCount,
} from "./fakeWindow";

export const CONVERSATION_URL = "https://chatgpt.com/backend-api/f/conversation";

// ─── answer stream ────────────────────────────────────────────────────────

/**
 * One SSE response of the conversation endpoint, written by the test. The
 * helpers speak the delta encoding sseParser.ts reads.
 */
export class AnswerStream {
  private controller!: ReadableStreamDefaultController<Uint8Array>;
  private encoder = new TextEncoder();
  private closed = false;
  readonly response: Response;
  // What the page read from its side of the (teed) body.
  pageRead = "";
  // When `close()` / `done()` ended it (epoch ms), 0 while open.
  endedAt = 0;

  constructor() {
    const body = new ReadableStream<Uint8Array>({
      start: (c) => {
        this.controller = c;
      },
    });
    this.response = new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
  }

  get open(): boolean {
    return !this.closed;
  }

  /** One SSE event carrying `data` (an object is sent as JSON). */
  event(data: object | string): void {
    if (this.closed) return;
    const payload = typeof data === "string" ? data : JSON.stringify(data);
    this.controller.enqueue(this.encoder.encode(`data: ${payload}\n\n`));
  }

  /** A new assistant `text` message addressed to everyone. */
  startAnswer(id: string, text = ""): void {
    this.event({
      p: "",
      o: "add",
      v: {
        message: {
          id,
          author: { role: "assistant" },
          content: { content_type: "text", parts: [text] },
          recipient: "all",
        },
      },
    });
  }

  append(text: string): void {
    this.event({ p: "/message/content/parts/0", o: "append", v: text });
  }

  /** The answer's own completion marker (`end_turn`). */
  endTurn(): void {
    this.event({ p: "/message/end_turn", o: "replace", v: true });
  }

  /** `[DONE]` and the end of the body. */
  done(): void {
    this.event("[DONE]");
    this.close();
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.endedAt = Date.now();
    this.controller.close();
  }

  /**
   * The body breaks off: reads end with `err`, by default the browser's
   * error for a network cut (live 28.10: no `message_stream_complete`, no
   * `[DONE]`). An aborted fetch ends with an AbortError instead.
   */
  cut(err: unknown = new TypeError("network error")): void {
    if (this.closed) return;
    this.closed = true;
    this.endedAt = Date.now();
    this.controller.error(err);
  }
}

/**
 * The page starts an answer stream: POSTs the conversation endpoint through
 * `window.fetch` (the shim's wrapper, when installed) and reads its side of
 * the body to the end, as React does.
 */
export function pageFetchesAnswerStream(): AnswerStream {
  const stream = new AnswerStream();
  let served = false;
  // One-shot: this stream answers the next conversation fetch only.
  routeFetch((url) => {
    if (served || url !== CONVERSATION_URL) return null;
    served = true;
    return stream.response;
  });
  void win.fetch(CONVERSATION_URL, { method: "POST" }).then(async (res) => {
    const reader = res.body?.getReader();
    if (!reader) return;
    const decoder = new TextDecoder();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        stream.pageRead += decoder.decode(value, { stream: true });
      }
    } catch {
      /* the page aborts its fetch after [DONE] */
    }
  });
  return stream;
}

// ─── page timeline ────────────────────────────────────────────────────────

// Page actions scheduled with `later`, so a test can wait for the page to
// have done everything it was going to do before the next test starts.
const pending = new Set<Promise<void>>();

/** Run `fn` on the page `ms` from now. */
export function later(ms: number, fn: () => void): void {
  const p = new Promise<void>((resolve) =>
    setTimeout(() => {
      try {
        fn();
      } finally {
        resolve();
      }
    }, ms),
  );
  pending.add(p);
  void p.then(() => pending.delete(p));
}

/** Resolves once every action scheduled with `later` has run. */
export async function pageIdle(): Promise<void> {
  while (pending.size > 0) await Promise.all([...pending]);
}

/**
 * End whatever a test left running before the next one: close `streams`,
 * wait for the page's scheduled actions, the shim to go dormant and the
 * adapter to drop its listeners (the shim's own one stays).
 */
export async function settlePage(streams: ReadonlyArray<AnswerStream | null> = []): Promise<void> {
  for (const s of streams) s?.close();
  await pageIdle();
  await until(() => shimActive() !== true, "the shim to go dormant");
  await until(() => windowListenerCount("message") === 1, "the adapter to drop its listeners");
}

function para(text: string): FakeElement {
  return h("p", { text });
}

// ─── "account" markup (signed in) ─────────────────────────────────────────

const ACCOUNT = {
  composer: [
    'div.ProseMirror[contenteditable="true"][role="textbox"][data-composer-markdown]',
    "[data-composer-markdown]",
  ],
  send: ['button[type="submit"]', 'main form button[type="submit"]'],
  stop: ['button[aria-label*="Stop" i]'],
  turn: ["[data-talvt-turn-state]"],
  answer: ["[data-dil-source-message-id]", "[data-markdown-copy]"],
};

export interface PastTurn {
  id: string;
  text: string;
}

/** One account turn: the user's message and, once it starts, the answer. */
export class AccountTurn {
  readonly el: FakeElement;
  answer: FakeElement | null = null;

  constructor(
    private chat: AccountChat,
    readonly id: string,
    state: string,
  ) {
    this.el = h("div", { sels: ACCOUNT.turn, attrs: { "data-talvt-turn-state": state } });
  }

  /** Show `text` as the answer (replacing what was shown). */
  render(text: string): void {
    if (!this.answer) {
      this.answer = h("div", {
        sels: ACCOUNT.answer,
        attrs: { "data-dil-source-message-id": this.id, "data-markdown-copy": "" },
      });
      this.el.append(this.answer);
    }
    this.answer.replaceChildren(...text.split("\n\n").map(para));
  }

  /** The turn ends: state `complete` (or `cancelled`), Stop gives way to Send. */
  finish(state: "complete" | "cancelled" = "complete"): void {
    this.el.setAttribute("data-talvt-turn-state", state);
    this.chat.showSend();
  }
}

/**
 * The signed-in ChatGPT web app at `path`, with the chat's past turns.
 * By default a Send click takes the prompt (`sentPrompts`), empties the
 * composer and calls `onSend`; what the page does next is up to the test.
 */
export class AccountChat {
  readonly main: FakeElement;
  readonly thread: FakeElement;
  readonly form: FakeElement;
  readonly composer: FakeElement;
  readonly send: FakeElement;
  readonly stop: FakeElement;
  readonly sentPrompts: string[] = [];
  onSend: (prompt: string) => void = () => {};
  private turnSeq = 0;

  constructor(path: string, past: readonly PastTurn[] = []) {
    resetPage(path);
    this.thread = h("div");
    this.composer = h("div", {
      sels: ACCOUNT.composer,
      attrs: { contenteditable: "true", role: "textbox", "data-composer-markdown": "", class: "ProseMirror" },
    });
    this.send = h("button", { sels: ACCOUNT.send, attrs: { type: "submit", "aria-label": "Send prompt" } });
    this.stop = h("button", { sels: ACCOUNT.stop, attrs: { type: "button", "aria-label": "Stop streaming" } });
    this.form = h("form", { children: [this.composer, this.send] });
    this.main = h("main", { children: [this.thread, this.form] });
    doc.body.append(this.main);
    for (const t of past) this.addPastTurn(t);
    this.send.onClick = () => this.takePrompt();
    // Enter in the composer is consumed (preventDefault) and sends only
    // while Send is shown, as ProseMirror does.
    this.composer.addEventListener("keydown", (ev) => {
      const e = ev as KeyboardEvent;
      if (e.key !== "Enter") return;
      e.preventDefault();
      if (this.send.isConnected) this.takePrompt();
    });
  }

  get composerText(): string {
    return this.composer.textContent;
  }

  private takePrompt(): void {
    const prompt = this.composerText;
    if (prompt.trim() === "") return;
    this.sentPrompts.push(prompt);
    this.composer.textContent = "";
    this.onSend(prompt);
  }

  addPastTurn(t: PastTurn): AccountTurn {
    const turn = new AccountTurn(this, t.id, "complete");
    turn.render(t.text);
    this.thread.append(turn.el);
    return turn;
  }

  /** The page starts a turn for the prompt just sent: `in_progress`, Stop instead of Send. */
  beginTurn(): AccountTurn {
    this.turnSeq++;
    const turn = new AccountTurn(this, `new-${this.turnSeq}`, "in_progress");
    this.thread.append(turn.el);
    this.showStop();
    return turn;
  }

  showStop(): void {
    this.send.remove();
    this.form.append(this.stop);
  }

  showSend(): void {
    this.stop.remove();
    if (!this.send.isConnected) this.form.append(this.send);
  }

  /** Another chat opened in this page (sidebar click): new path, its turns only. */
  showOtherChat(path: string, past: readonly PastTurn[]): void {
    goTo(path);
    this.thread.replaceChildren();
    for (const t of past) this.addPastTurn(t);
    this.showSend();
  }
}

// ─── "octane" markup (signed out) ─────────────────────────────────────────

const OCTANE = {
  host: ["[data-composer-editor-host]"],
  composer: ['textarea[name="prompt"]'],
  submit: ["button[data-composer-submit]"],
  stopByLabel: 'button[aria-label*="Stop" i]',
  user: ['[data-message-role="user"]'],
  assistant: ['[data-message-role="assistant"]'],
  markdown: ["[data-assistant-markdown-root]"],
  streaming: ["[data-message-streaming]"],
};

/**
 * The signed-out front-end: a textarea composer and one submit button that
 * turns into Stop while streaming. Its chats never get a saved address.
 */
export class OctaneChat {
  readonly thread: FakeElement;
  readonly composer: FakeTextArea;
  readonly submit: FakeElement;
  readonly sentPrompts: string[] = [];
  onSend: (prompt: string) => void = () => {};

  constructor(path = "/") {
    resetPage(path);
    this.thread = h("div");
    this.composer = new FakeTextArea({ sels: OCTANE.composer, attrs: { name: "prompt" } });
    this.submit = h("button", {
      sels: OCTANE.submit,
      attrs: { "data-composer-submit": "", "aria-label": "Send", "data-send-label": "Send", "data-stop-label": "Stop" },
    });
    const host = h("div", { sels: OCTANE.host, children: [this.composer] });
    doc.body.append(h("main", { children: [this.thread, host, this.submit] }));
    this.submit.onClick = () => {
      if (this.submit.getAttribute("aria-label") !== "Send") return;
      const prompt = this.composer.value;
      if (prompt.trim() === "") return;
      this.sentPrompts.push(prompt);
      this.composer.value = "";
      this.onSend(prompt);
    };
  }

  /** Show the user's message and an answer that streams in as `text`, then ends. */
  answer(prompt: string, text: string, ms = 200): void {
    this.thread.append(h("div", { sels: OCTANE.user, text: prompt }));
    const root = h("div", { sels: OCTANE.markdown, children: [para("")] });
    const msg = h("div", { sels: [...OCTANE.assistant, ...OCTANE.streaming], children: [root] });
    this.thread.append(msg);
    this.setStreaming(true);
    later(ms / 2, () => root.replaceChildren(para(text.slice(0, Math.ceil(text.length / 2)))));
    later(ms, () => {
      root.replaceChildren(para(text));
      msg.sels.delete(OCTANE.streaming[0] as string);
      this.setStreaming(false);
    });
  }

  private setStreaming(on: boolean): void {
    this.submit.setAttribute("aria-label", on ? "Stop" : "Send");
    if (on) this.submit.sels.add(OCTANE.stopByLabel);
    else this.submit.sels.delete(OCTANE.stopByLabel);
  }
}
