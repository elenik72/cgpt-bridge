// The stream ↔ page check and the stream capture (T2), on the fake window
// with the real pageShim.ts and content.ts. Both only report: the answer an
// ask returns is the one it took, whatever the page shows, and neither adds
// any wait to the ask.

import { test } from "node:test";
import assert from "node:assert/strict";

import { ask, loadPage } from "./harness/fakeWindow";
import { AccountChat, later, pageFetchesAnswerStream, settlePage, type AnswerStream } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";
import { answerSkeleton, compareStreamAndPage, takeStreamCapture } from "../src/chatgptAdapter";

await loadPage();
useAutoClock();

const CHAT = "https://chatgpt.com/c/abc";
const TARGET = { target: { kind: "conversation", url: CHAT } };
const PAST = [{ id: "m0", text: "An earlier answer." }];

/**
 * Send starts a turn whose stream says `pieces` (joined) and whose page
 * shows `shown`: the page at `domMs` (finished), the stream's end at 100 ms.
 */
function answerWith(
  chat: AccountChat,
  pieces: string[],
  shown: string,
  domMs = 50,
): () => AnswerStream | null {
  let stream = null as AnswerStream | null;
  chat.onSend = () => {
    const turn = chat.beginTurn();
    const s = pageFetchesAnswerStream();
    stream = s;
    later(domMs, () => {
      turn.render(shown);
      turn.finish();
    });
    later(100, () => {
      s.startAnswer("m1");
      for (const p of pieces) s.append(p);
      s.endTurn();
    });
    later(Math.max(domMs, 100) + 50, () => s.done());
  };
  return () => stream;
}

test("streamCheck.skeleton_drops_what_the_rendering_drops", () => {
  const source =
    "Here is **the** answer, see [the docs](https://example.invalid/a_(b)).\n\n" +
    "1. first\n- second\n\n---\n\n" +
    '```tool_calls id="call-1"\n[{"name": "read"}]\n```\n\\*not bold\\*';
  const rendered =
    "Here is the answer, see the docs.\nfirst\nsecond\n\n```tool_calls\n" +
    '[{"name": "read"}]\n```\n*not bold*';
  assert.equal(answerSkeleton(source), answerSkeleton(rendered));
  assert.deepEqual(compareStreamAndPage(source, rendered).mismatch, false);
  // The same answer twice is not the same answer.
  const doubled = compareStreamAndPage("Answer.", "Answer.\n\nAnswer.");
  assert.deepEqual(doubled, { streamChars: 7, pageChars: 14, mismatch: true });
});

test("streamCheck.a_page_that_shows_the_answer_twice_is_reported_and_the_stream_text_is_kept", async () => {
  const chat = new AccountChat("/c/abc", PAST);
  const stream = answerWith(chat, ["Reading the file now."], "Reading the file now.\n\nReading the file now.");
  const reply = await ask("s1", "hello", TARGET);
  assert.equal(reply.text, "Reading the file now.", JSON.stringify(reply));
  assert.deepEqual(reply.diagnostics, {
    streamCheck: { streamChars: 18, pageChars: 36, mismatch: true },
  });
  await settlePage([stream()]);
});

test("streamCheck.a_page_that_renders_the_same_answer_reports_nothing", async () => {
  const chat = new AccountChat("/c/abc", PAST);
  const stream = answerWith(chat, ["Here is **the** answer."], "Here is the answer.");
  const reply = await ask("s2", "hello", TARGET);
  assert.equal(reply.text, "Here is **the** answer.");
  assert.equal(reply.diagnostics, undefined, JSON.stringify(reply));
  await settlePage([stream()]);
});

test("streamCheck.a_page_behind_the_stream_is_no_check_and_adds_no_wait", async () => {
  const chat = new AccountChat("/c/abc", PAST);
  // The page shows the (different) answer only 5 s after the stream ended.
  const stream = answerWith(chat, ["Fast stream."], "Fast stream.\n\nFast stream.", 5_000);
  const started = Date.now();
  const reply = await ask("s3", "hello", TARGET);
  const took = Date.now() - started;
  assert.equal(reply.text, "Fast stream.");
  assert.equal(reply.diagnostics, undefined, JSON.stringify(reply));
  assert.ok(took < 1_000, `answered after ${took} ms`);
  await settlePage([stream()]);
});

test("streamCheck.a_cli_ask_is_never_checked", async () => {
  const chat = new AccountChat("/c/abc", PAST);
  const stream = answerWith(chat, ["CLI answer."], "CLI answer.\n\nCLI answer.");
  const reply = await ask("s4", "hello");
  assert.equal(reply.text, "CLI answer.", JSON.stringify(reply));
  assert.equal(reply.diagnostics, undefined, JSON.stringify(reply));
  await settlePage([stream()]);
});

test("streamCheck.capture_stream_keeps_text_pieces_joined_and_only_when_asked", async () => {
  const chat = new AccountChat("/c/abc", PAST);
  // A secret split over three events.
  let stream = answerWith(chat, ["Key: AKIAIOSF", "ODNN7EX", "AMPLE done."], "Key: AKIAIOSFODNN7EXAMPLE done.");
  const reply = await ask("s5", "hello", { ...TARGET, captureStream: true });
  assert.equal(reply.text, "Key: AKIAIOSFODNN7EXAMPLE done.");
  const raw = reply.diagnostics?.streamCapture ?? "";
  // Every event is kept, its text pieces as references...
  assert.ok(raw.startsWith("data: "), raw.slice(0, 80));
  assert.ok(raw.includes('"o":"append","v":"«text 0+13»"'), raw);
  assert.ok(raw.includes('"v":"«text 13+7»"'), raw);
  assert.ok(!raw.includes('"v":"ODNN7EX"'), raw);
  // ...and the pieces joined once at the end, where redaction sees them whole.
  const [, joined] = raw.split("«text OFF+LEN» above points here\n");
  assert.equal(joined, "Key: AKIAIOSFODNN7EXAMPLE done.");
  await settlePage([stream()]);

  // Without the flag the shim keeps nothing and nothing comes back.
  stream = answerWith(chat, ["Plain answer."], "Plain answer.");
  const plain = await ask("s6", "hello again", TARGET);
  assert.equal(plain.text, "Plain answer.");
  assert.equal(plain.diagnostics, undefined, JSON.stringify(plain));
  await settlePage([stream()]);
});

test("streamCheck.a_shim_that_did_not_acknowledge_capture_is_not_waited_for", async () => {
  // An epoch whose "on" carried no capture acknowledgement (a page script
  // from before captures): no take, no 500 ms wait, one warning.
  const warned: string[] = [];
  const warn = console.warn;
  console.warn = (...a: unknown[]) => void warned.push(a.join(" "));
  try {
    const started = Date.now();
    assert.equal(await takeStreamCapture(987_654), null);
    assert.ok(Date.now() - started < 100, `${Date.now() - started} ms`);
  } finally {
    console.warn = warn;
  }
  assert.ok(warned.some((w) => w.includes("too old for capture")), warned.join("\n"));
});
