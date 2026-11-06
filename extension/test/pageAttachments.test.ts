// T6: the files of a gateway ask (images from OpenCode) go into the composer
// before the text, and Send is pressed only once every upload has ended. On
// the fake window with snapshot HTML (the adapter's selectors are evaluated
// against the attributes), "account" markup. The attachment markup here is
// the adapter's GUESS (see the inventory at the top of chatgptAdapter.ts):
// a file input in the composer form, a tile per file with a `blob:` preview,
// a progress bar while it uploads and a "Remove file" button. It was not
// recorded live; these tests pin what the adapter does with that markup, not
// that ChatGPT shows it. Synthetic markup and bytes only.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, doc, loadPage, resetPage } from "./harness/fakeWindow";
import type { FakeElement } from "./harness/fakeWindow";
import { loadSnapshot, parseHtml } from "./harness/domSnapshot";
import { later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";

await loadPage();
useAutoClock();

// What the page script gets from `new DataTransfer()` (Node has none).
class FakeDataTransfer {
  readonly files: File[] = [];
  readonly items = { add: (file: File): void => void this.files.push(file) };
}

class FakeClipboardEvent extends Event {
  readonly clipboardData: FakeDataTransfer | null;
  constructor(type: string, init: EventInit & { clipboardData?: FakeDataTransfer } = {}) {
    super(type, init);
    this.clipboardData = init.clipboardData ?? null;
  }
}

const g = globalThis as unknown as Record<string, unknown>;
g.DataTransfer = FakeDataTransfer;
g.ClipboardEvent = FakeClipboardEvent;

// A 1×1 PNG as the gateway sends it.
const PNG_BASE64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==";
const PNG_FILE = { name: "image-1.png", mime: "image/png", dataBase64: PNG_BASE64 };
const CHAT = "https://chatgpt.com/c/abc";

interface Layout {
  // A file input in the composer's form.
  fileInput: boolean;
  // A project's own file upload outside the composer: an input and a tile
  // with a "Remove file" button.
  outside?: boolean;
  // The composer sits in a plain container, not a form.
  noForm?: boolean;
}

// The chat shows one earlier exchange (a conversation is open only once its
// turns are rendered).
const page = (l: Layout): string => `
<main>
  ${
    l.outside
      ? '<section data-project-files=""><input type="file" multiple="" data-outside=""><div data-outside-tile=""><button type="button" aria-label="Remove file 1: spec.pdf"></button></div></section>'
      : ""
  }
  <div data-thread=""><section data-talvt-turn-state="complete"><div data-user-message-bubble=""><div>Earlier</div></div></section><section data-talvt-turn-state="complete"><div data-dil-source-message-id="m0"><div data-markdown-copy=""><p data-d-text-flow="">Earlier answer.</p></div></div></section></div>
  <${l.noForm ? "div" : "form"} data-composer-box="">
    <div data-tiles=""></div>
    ${l.fileInput ? '<input type="file" multiple="" accept="image/*,.pdf" tabindex="-1" data-composer-input="">' : ""}
    <div contenteditable="true" role="textbox" data-composer-markdown="" aria-label="Ask anything"></div>
    <button type="submit" aria-label="Send prompt"></button>
  </${l.noForm ? "div" : "form"}>
</main>`;

function el(selector: string): FakeElement {
  const found = doc.querySelector(selector);
  if (!found) throw new Error(`not in the snapshot: ${selector}`);
  return found;
}

const tiles = (): FakeElement[] => el("[data-tiles]").querySelectorAll("[data-tile]");

interface Shown {
  // Every prompt sent, with the files (names) and whether an upload still
  // ran at that moment.
  sent: Array<{ prompt: string; files: string[]; uploading: boolean }>;
  // The files the page was handed, in order.
  received: File[];
  // Files handed to the project's own upload (`outside`), and presses of
  // its remove button.
  outsideReceived: number;
  outsideRemoved: number;
}

/**
 * The chat at /c/abc. Files handed to the page (the file input's `change`,
 * or a paste on the composer) each get a tile with a progress bar that ends
 * after `uploadMs` (never when null); the tile's remove button takes it out.
 * `ignoreFiles`: the page shows nothing for them (markup the adapter no
 * longer knows). Send takes the prompt and the tiles and answers "Seen."
 */
function showChat(layout: Layout | boolean, uploadMs: number | null, ignoreFiles = false): Shown {
  const l: Layout = typeof layout === "boolean" ? { fileInput: layout } : layout;
  resetPage("/c/abc");
  loadSnapshot(page(l));
  const shown: Shown = { sent: [], received: [], outsideReceived: 0, outsideRemoved: 0 };
  if (l.outside) {
    const input = el("[data-outside]");
    input.addEventListener("change", () => {
      shown.outsideReceived += (input as unknown as { files: File[] }).files.length;
    });
    el("[data-outside-tile] button").onClick = () => void shown.outsideRemoved++;
  }
  const take = (files: readonly File[]): void => {
    if (ignoreFiles) return;
    for (const file of files) {
      shown.received.push(file);
      const [tile] = parseHtml(
        `<div data-tile=""><img src="blob:https://chatgpt.com/${shown.received.length}" alt="">` +
          `<div role="progressbar"></div>` +
          `<button type="button" aria-label="Remove file ${shown.received.length}: ${file.name}"></button></div>`,
      ) as FakeElement[];
      tile!.querySelector("button")!.onClick = () => tile!.remove();
      el("[data-tiles]").append(tile!);
      if (uploadMs !== null) later(uploadMs, () => tile!.querySelector('[role="progressbar"]')?.remove());
    }
  };
  if (l.fileInput) {
    const input = el("[data-composer-input]");
    input.addEventListener("change", () => take((input as unknown as { files: File[] }).files));
  }
  const composer = el('[role="textbox"]');
  composer.addEventListener("paste", (ev) => {
    const data = (ev as FakeClipboardEvent).clipboardData;
    if (data) take(data.files);
  });
  const thread = el("[data-thread]");
  el('button[type="submit"]').onClick = () => {
    const prompt = composer.textContent;
    if (prompt.trim() === "") return;
    shown.sent.push({
      prompt,
      files: tiles().map((t) => t.querySelector("button")!.getAttribute("aria-label")!),
      uploading: el("[data-tiles]").querySelector('[role="progressbar"]') !== null,
    });
    composer.textContent = "";
    el("[data-tiles]").replaceChildren();
    thread.append(
      ...parseHtml(
        `<section data-talvt-turn-state="in_progress"><div data-user-message-bubble=""><div>${prompt}</div></div>` +
          `<div data-dil-source-message-id="m1"><div data-markdown-copy=""></div></div></section>`,
      ),
    );
    later(200, () => {
      thread.querySelectorAll("[data-markdown-copy]").at(-1)!.append(...parseHtml('<p data-d-text-flow="">Seen.</p>'));
      thread.querySelectorAll("[data-talvt-turn-state]").at(-1)!.setAttribute("data-talvt-turn-state", "complete");
    });
  };
  return shown;
}

const askWithFiles = (id: string, files: unknown[], timeoutMs = 120_000) =>
  ask(id, "What is on the screenshot?", {
    target: { kind: "conversation", url: CHAT },
    attachments: files,
    timeoutMs,
  });

test("attachments.files_upload_before_the_text_is_sent", async () => {
  try {
    const shown = showChat(true, 3_000);
    const reply = await askWithFiles("f1", [PNG_FILE]);
    assert.deepEqual(reply, { id: "f1", ok: true, kind: "test.ask", text: "Seen.", conversationUrl: CHAT });
    assert.deepEqual(shown.sent, [
      { prompt: "What is on the screenshot?", files: ["Remove file 1: image-1.png"], uploading: false },
    ]);
    // The page got the bytes as a file of that name and type.
    const [file] = shown.received;
    assert.equal(file?.name, "image-1.png");
    assert.equal(file?.type, "image/png");
    const bytes = new Uint8Array(await file!.arrayBuffer());
    assert.deepEqual([...bytes.slice(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    assert.equal(Buffer.from(bytes).toString("base64"), PNG_BASE64);
  } finally {
    await settlePage();
  }
});

test("attachments.an_upload_that_never_ends_fails_and_nothing_is_sent", async () => {
  try {
    const shown = showChat(true, null);
    const reply = await askWithFiles("f2", [PNG_FILE]);
    assert.equal(reply.ok, false);
    assert.equal(reply.errorCode, "attachment_upload_failed");
    assert.match(reply.message ?? "", /^attachment_upload_failed: .*within 60 s.*not sent/);
    assert.deepEqual(shown.sent, []);
    // The file is taken out again: the next Send must not carry it.
    assert.equal(tiles().length, 0);
  } finally {
    await settlePage();
  }
});

test("attachments.a_page_that_shows_no_tile_fails_and_nothing_is_sent", async () => {
  try {
    const shown = showChat(true, 1_000, true);
    const reply = await askWithFiles("f3", [PNG_FILE]);
    assert.equal(reply.errorCode, "attachment_upload_failed");
    assert.match(reply.message ?? "", /showed 0 of 1 attached files/);
    assert.deepEqual(shown.sent, []);
  } finally {
    await settlePage();
  }
});

test("attachments.without_a_file_input_the_files_are_pasted_on_the_composer", async () => {
  try {
    const shown = showChat(false, 500);
    const reply = await askWithFiles("f4", [PNG_FILE, { ...PNG_FILE, name: "image-2.png" }]);
    assert.equal(reply.ok, true, reply.message);
    assert.deepEqual(shown.sent, [
      {
        prompt: "What is on the screenshot?",
        files: ["Remove file 1: image-1.png", "Remove file 2: image-2.png"],
        uploading: false,
      },
    ]);
  } finally {
    await settlePage();
  }
});

test("attachments.files_a_failed_ask_left_are_removed_before_the_next_gateway_ask", async () => {
  try {
    const shown = showChat(true, 0);
    // A leftover tile from an earlier ask.
    const [left] = parseHtml(
      '<div data-tile=""><img src="blob:https://chatgpt.com/old" alt=""><button type="button" aria-label="Remove file 1: old.png"></button></div>',
    ) as FakeElement[];
    left!.querySelector("button")!.onClick = () => left!.remove();
    el("[data-tiles]").append(left!);
    const reply = await ask("f5", "Plain text", { target: { kind: "conversation", url: CHAT } });
    assert.equal(reply.ok, true, reply.message);
    assert.deepEqual(shown.sent, [{ prompt: "Plain text", files: [], uploading: false }]);
  } finally {
    await settlePage();
  }
});

test("attachments.bad_files_never_reach_the_page", async () => {
  try {
    const shown = showChat(true, 0);
    const reply = (await askWithFiles("f6", [{ ...PNG_FILE, mime: "text/html" }])) as unknown;
    // The page does not take such a request at all.
    assert.notEqual((reply as { ok?: boolean }).ok, true);
    assert.deepEqual(shown.sent, []);
    assert.equal(shown.received.length, 0);
  } finally {
    await settlePage();
  }
});

// Files another session's ask left that cannot be removed would go into this
// chat with the next Send: the ask is refused before anything is typed.
test("attachments.leftover_files_that_cannot_be_removed_refuse_the_ask", async () => {
  for (const files of [[], [PNG_FILE]]) {
    try {
      const shown = showChat(true, 0);
      const [left] = parseHtml(
        '<div data-tile=""><img src="blob:https://chatgpt.com/old" alt=""><button type="button" aria-label="Remove file 1: old.png"></button></div>',
      ) as FakeElement[];
      // Its remove button does nothing.
      el("[data-tiles]").append(left!);
      const reply = await askWithFiles(`lo-${files.length}`, files);
      assert.equal(reply.ok, false);
      assert.equal(reply.errorCode, "attachments_left_over");
      assert.match(reply.message ?? "", /^attachments_left_over: .*not sent/);
      assert.deepEqual(shown.sent, []);
      assert.equal(shown.received.length, 0, "no file of this ask was put in");
      assert.equal(el('[role="textbox"]').textContent, "", "nothing was typed");
    } finally {
      await settlePage();
    }
  }
});

// A project page has its own file upload and "Remove file" buttons outside
// the composer: the files never go there, and those buttons are never pressed.
test("attachments.an_input_outside_the_composer_is_never_used", async () => {
  try {
    const shown = showChat({ fileInput: false, outside: true }, 500);
    const reply = await askWithFiles("o1", [PNG_FILE]);
    assert.equal(reply.ok, true, reply.message);
    assert.equal(shown.outsideReceived, 0);
    assert.equal(shown.outsideRemoved, 0);
    // The composer got them by a paste instead.
    assert.deepEqual(shown.sent, [
      { prompt: "What is on the screenshot?", files: ["Remove file 1: image-1.png"], uploading: false },
    ]);
  } finally {
    await settlePage();
  }
});

test("attachments.a_composer_without_its_own_form_gets_no_files_and_nothing_is_sent", async () => {
  try {
    const shown = showChat({ fileInput: true, outside: true, noForm: true }, 0);
    const reply = await askWithFiles("o2", [PNG_FILE]);
    assert.equal(reply.errorCode, "attachment_upload_failed");
    assert.match(reply.message ?? "", /no form of its own/);
    assert.deepEqual(shown.sent, []);
    assert.equal(shown.received.length, 0);
    assert.equal(shown.outsideReceived, 0);
    assert.equal(shown.outsideRemoved, 0);
  } finally {
    await settlePage();
  }
});

test("attachments.no_time_left_to_upload_is_a_timeout", async () => {
  try {
    const shown = showChat(true, 0);
    // The deadline is already at the time the prompt needs to be sent.
    const reply = await ask("t1", "What is on the screenshot?", {
      target: { kind: "conversation", url: CHAT },
      attachments: [PNG_FILE],
      timeoutMs: 30_000,
      deadlineMs: Date.now() + 15_000,
    });
    assert.equal(reply.ok, false);
    assert.equal(reply.errorCode, "answer_timeout");
    assert.deepEqual(shown.sent, []);
    assert.equal(shown.received.length, 0);
  } finally {
    await settlePage();
  }
});

test("attachments.files_capture_and_stream_check_together_in_one_ask", async () => {
  let stream = null as AnswerStream | null;
  try {
    const shown = showChat(true, 100);
    // The page shows "Seen." (at 200 ms); its answer stream says more and
    // ends after that, so the page is settled when the stream is done.
    const submit = el('button[type="submit"]');
    const pageSend = submit.onClick;
    submit.onClick = () => {
      pageSend?.();
      const s = pageFetchesAnswerStream();
      stream = s;
      later(300, () => {
        s.startAnswer("m1");
        s.append("Seen: a button.");
        s.endTurn();
      });
      later(400, () => s.done());
    };
    const reply = await ask("fc1", "What is on the screenshot?", {
      target: { kind: "conversation", url: CHAT },
      attachments: [PNG_FILE],
      captureStream: true,
    });
    assert.equal(reply.text, "Seen: a button.", JSON.stringify(reply));
    assert.deepEqual(shown.sent[0]?.files, ["Remove file 1: image-1.png"]);
    assert.deepEqual(reply.diagnostics?.streamCheck, { streamChars: 13, pageChars: 5, mismatch: true });
    const capture = reply.diagnostics?.streamCapture ?? "";
    assert.ok(capture.includes('"v":"«text 0+15»"'), capture);
    assert.ok(capture.endsWith("above points here\nSeen: a button."), capture);
  } finally {
    await settlePage([stream]);
  }
});
