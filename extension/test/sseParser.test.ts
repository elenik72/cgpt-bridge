// Pins how sseParser.ts turns ChatGPT conversation streams into answer text.
//
// The fixtures in test/fixtures/sse/*.sse are synthetic: assembled by hand in
// the formats the parser documents (delta_encoding with append / replace /
// truncate, patch batches, bare continuations, reasoning and search messages,
// end_turn, [DONE], legacy snapshots). They contain no recorded conversations.
// If ChatGPT changes its stream format and the answer text on real traffic
// stops matching, update the fixtures together with the parser.
//
// Tests marked `CHARACTERIZATION (...)` pin the CURRENT behaviour of the
// parser, including quirks that a named later task may change on purpose.
// They are a snapshot, not a contract: when that task lands, update the test
// together with it instead of treating the red test as a regression.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import {
  SseFramer,
  answerText,
  captureEvent,
  isComplete,
  newSseState,
  newStreamCapture,
  renderCapture,
  sseAccumulate,
  type SseState,
} from "../src/sseParser";

// `npm test` runs from extension/ (the bundle lives in .test-build/, so the
// fixtures are read from the source tree rather than imported).
const FIXTURE_DIR = resolve(process.cwd(), "test", "fixtures", "sse");
const fixture = (name: string): string => readFileSync(resolve(FIXTURE_DIR, name), "utf8");

const CHUNK_SIZES = [1, 7, 4096];

/** Mirrors pageShim: byte chunks -> streaming TextDecoder -> framer -> accumulator. */
function parse(body: string, chunkBytes: number): { state: SseState; events: string[] } {
  const bytes = new TextEncoder().encode(body);
  const decoder = new TextDecoder();
  const framer = new SseFramer();
  const state = newSseState();
  const events: string[] = [];
  const feed = (evs: string[]) => {
    for (const ev of evs) {
      events.push(ev);
      sseAccumulate(state, ev);
    }
  };
  for (let i = 0; i < bytes.length; i += chunkBytes) {
    feed(framer.push(decoder.decode(bytes.subarray(i, i + chunkBytes), { stream: true })));
  }
  feed(framer.push(decoder.decode()));
  feed(framer.flush());
  return { state, events };
}

/** Parses the fixture whole and in 1/7/4096-byte chunks; all must agree. */
function parseAllChunkings(name: string): SseState {
  const body = fixture(name);
  const whole = parse(body, new TextEncoder().encode(body).length);
  for (const size of CHUNK_SIZES) {
    const chunked = parse(body, size);
    assert.deepEqual(chunked.events, whole.events, `${name}: events differ at ${size}-byte chunks`);
    assert.deepEqual(chunked.state, whole.state, `${name}: state differs at ${size}-byte chunks`);
    assert.equal(answerText(chunked.state), answerText(whole.state), `${name}: answer differs at ${size}-byte chunks`);
  }
  return whole.state;
}

test("sseParser.delta_append_with_bare_continuations", () => {
  const state = parseAllChunkings("delta-append.sse");
  // Includes Cyrillic and an emoji, so 1- and 7-byte chunks split characters.
  assert.equal(answerText(state), "Hello, world! Привет мир 👋.");
  assert.deepEqual(
    state.messages.map((m) => [m.id, m.role, m.contentType]),
    [
      ["msg-user-1", "user", "text"],
      ["msg-asst-1", "assistant", "text"],
    ],
  );
  assert.equal(state.messages[0]?.text, "Say hello"); // the user's own text is kept but never returned
  assert.equal(isComplete(state), true);
});

test("sseParser.delta_replace_and_truncate_rewrite_the_text", () => {
  const state = parseAllChunkings("delta-replace-truncate.sse");
  assert.equal(answerText(state), "Final answer, edited");
  assert.equal(isComplete(state), true);
});

test("sseParser.bare_value_after_replace_replaces_again", () => {
  // CHARACTERIZATION (no planned task; changes only if a live capture, TASK-016, shows ChatGPT
  // appends here): a bare `{ v }` repeats the previous operation, so after a replace it replaces.
  const P = "/message/content/parts/0";
  const state = newSseState();
  sseAccumulate(state, JSON.stringify({ p: "", o: "add", v: { message: { id: "a", author: { role: "assistant" }, content: { content_type: "text", parts: [""] } } } }));
  sseAccumulate(state, JSON.stringify({ p: P, o: "replace", v: "one" }));
  sseAccumulate(state, JSON.stringify({ v: "two" }));
  assert.equal(answerText(state), "two");
});

test("sseParser.patch_batches_with_bare_sub_ops_and_bare_batches", () => {
  const state = parseAllChunkings("patch-batches.sse");
  assert.equal(answerText(state), "Batched text arrives in patches.");
  assert.equal(state.messages.length, 1);
  assert.equal(state.messages[0]?.endTurn, true);
  assert.equal(isComplete(state), true);
});

test("sseParser.reasoning_and_search_messages_are_not_the_answer", () => {
  const state = parseAllChunkings("reasoning-search.sse");
  assert.equal(answerText(state), "Based on the search, the answer is 42.");
  assert.deepEqual(
    state.messages.map((m) => [m.id, m.role, m.contentType, m.recipient]),
    [
      ["msg-user-1", "user", "text", "all"],
      ["msg-think-1", "assistant", "thoughts", "all"],
      ["msg-search-1", "assistant", "code", "web.run"],
      ["msg-tool-1", "tool", "text", "all"],
      ["msg-pre-1", "assistant", "text", "web.run"],
      ["msg-asst-1", "assistant", "text", "all"],
    ],
  );
  // Text appended while a reasoning message was current stays on that message.
  assert.equal(state.messages[1]?.text, " harder about it");
  assert.equal(state.messages[4]?.text, "query for the tool, not the user");
});

test("sseParser.reasoning_and_tool_messages_alone_give_an_empty_answer", () => {
  // Cut the stream at the event that starts the final assistant message.
  const { events } = parse(fixture("reasoning-search.sse"), 4096);
  const cut = events.findIndex((ev) => ev.includes("msg-asst-1"));
  assert.ok(cut > 0);
  assert.ok(events.slice(0, cut).some((ev) => ev.includes("msg-search-1")));
  const state = newSseState();
  for (const ev of events.slice(0, cut)) sseAccumulate(state, ev);
  assert.ok(state.messages.length >= 5);
  assert.equal(answerText(state), "");
});

test("sseParser.end_turn_completes_only_the_assistant_text_answer", () => {
  const open = parseAllChunkings("end-turn-open.sse");
  assert.equal(answerText(open), "Answer before end_turn");
  assert.equal(isComplete(open), false); // an earlier message with end_turn=false does not complete it

  const closed = parseAllChunkings("end-turn-closed.sse");
  assert.equal(answerText(closed), "Answer before end_turn");
  assert.equal(isComplete(closed), true); // no [DONE] and no stream_complete in this stream
});

test("sseParser.end_turn_on_a_non_assistant_or_non_text_message_does_not_complete", () => {
  const P = "/message/content/parts/0";
  const message = (id: string, role: string, contentType: string) => ({
    p: "",
    o: "add",
    v: { message: { id, author: { role }, content: { content_type: contentType, parts: [""] }, recipient: "all" } },
  });
  const endTurn = { p: "/message/end_turn", o: "replace", v: true };
  for (const [role, ct] of [
    ["user", "text"],
    ["assistant", "thoughts"],
    ["tool", "text"],
  ] as const) {
    const state = newSseState();
    sseAccumulate(state, JSON.stringify(message("m", role, ct)));
    sseAccumulate(state, JSON.stringify({ p: P, o: "append", v: "x" }));
    sseAccumulate(state, JSON.stringify(endTurn));
    assert.equal(isComplete(state), false, `${role}/${ct}`);
  }
});

test("sseParser.done_marker_completes_the_stream", () => {
  const state = newSseState();
  sseAccumulate(state, JSON.stringify({ p: "", o: "add", v: { message: { id: "a", author: { role: "assistant" }, content: { content_type: "text", parts: ["Hi"] } } } }));
  assert.equal(isComplete(state), false);
  sseAccumulate(state, "[DONE]");
  assert.equal(isComplete(state), true);
  assert.equal(answerText(state), "Hi");
});

test("sseParser.message_stream_complete_completes_the_stream", () => {
  const state = newSseState();
  sseAccumulate(state, JSON.stringify({ type: "message_stream_complete", conversation_id: "c" }));
  assert.equal(isComplete(state), true);
});

test("sseParser.legacy_snapshots_replace_the_message_with_the_same_id", () => {
  const state = parseAllChunkings("legacy-snapshot.sse");
  assert.equal(answerText(state), "Hello, world!");
  assert.deepEqual(
    state.messages.map((m) => [m.id, m.role]),
    [
      ["msg-user-1", "user"],
      ["msg-asst-1", "assistant"],
    ],
  );
  assert.equal(isComplete(state), true); // via [DONE]
});

test("sseParser.legacy_snapshot_completion_comes_from_done_not_end_turn", () => {
  // CHARACTERIZATION (no planned task; revisit with TASK-015 / AUD-007 or TASK-016 if completion
  // semantics change): a legacy snapshot with end_turn=true does not complete the stream by itself;
  // only [DONE] does.
  const body = fixture("legacy-snapshot.sse");
  const withoutDone = body.slice(0, body.indexOf("data: [DONE]"));
  const { state } = parse(withoutDone, 4096);
  assert.equal(answerText(state), "Hello, world!");
  assert.equal(isComplete(state), false);
});

test("sseParser.legacy_snapshot_with_a_new_id_becomes_the_newest_answer", () => {
  const state = parseAllChunkings("legacy-two-messages.sse");
  assert.equal(answerText(state), "The real answer");
  assert.equal(state.messages.length, 2);
});

test("sseParser.stream_without_an_assistant_message_gives_an_empty_answer", () => {
  // CHARACTERIZATION (TASK-015 / AUD-007): the parser reports an empty answer with isComplete=true;
  // the adapter turns that into a fast answer_not_detected unless the DOM shows an answer.
  const state = parseAllChunkings("no-assistant.sse");
  assert.equal(answerText(state), "");
  assert.equal(state.messages.length, 1);
  assert.equal(state.messages[0]?.role, "user");
  assert.equal(isComplete(state), true); // [DONE] still ends the stream
});

test("sseParser.fresh_state_has_an_empty_answer_and_is_not_complete", () => {
  assert.equal(answerText(newSseState()), "");
  assert.equal(isComplete(newSseState()), false);
});

test("sseParser.accumulate_ignores_garbage_events", () => {
  // CHARACTERIZATION (TASK-016 / AUD-007): events such as errors or titles are ignored today;
  // TASK-016 may add recognition of an `error` field.
  const state = newSseState();
  for (const data of ["", "not json", "42", '"v1"', "[1,2]", "null", '{"type":"title_generation","title":"t"}']) {
    sseAccumulate(state, data);
  }
  assert.deepEqual(state, newSseState());
});

test("sseParser.appends_before_any_message_are_dropped", () => {
  const state = newSseState();
  sseAccumulate(state, JSON.stringify({ p: "/message/content/parts/0", o: "append", v: "orphan" }));
  sseAccumulate(state, JSON.stringify({ v: "more orphan" }));
  assert.equal(answerText(state), "");
  assert.equal(state.messages.length, 0);
});

test("sseParser.latest_assistant_text_message_wins", () => {
  // CHARACTERIZATION (AUD-019 known limitation, no planned task; TASK-010 / AUD-003 changes stream
  // binding in pageShim, not this selection): only the newest non-empty assistant text message is
  // returned, falling back to the previous one while the newest is still empty.
  const state = newSseState();
  const add = (id: string, text: string) =>
    JSON.stringify({ p: "", o: "add", v: { message: { id, author: { role: "assistant" }, content: { content_type: "text", parts: [text] }, recipient: "all" } } });
  sseAccumulate(state, add("a", "first"));
  sseAccumulate(state, add("b", "second"));
  assert.equal(answerText(state), "second");
  sseAccumulate(state, add("c", "")); // an empty newest message falls back to the previous text
  assert.equal(answerText(state), "second");
});

test("sseFramer.joins_multiline_data_and_handles_crlf", () => {
  const framer = new SseFramer();
  const out = [
    ...framer.push("event: delta\r\ndata: line one\r\ndata: line two\r\n\r\n"),
    ...framer.push("data:no-space\n\n"),
  ];
  assert.deepEqual(out, ["line one\nline two", "no-space"]);
});

test("sseFramer.ignores_comments_event_names_and_empty_events", () => {
  const framer = new SseFramer();
  const out = [
    ...framer.push(": keep-alive\n\n"),
    ...framer.push("event: ping\n\n"),
    ...framer.push("id: 7\nretry: 100\ndata: x\n\n"),
  ];
  assert.deepEqual(out, ["x"]);
});

test("sseFramer.holds_an_event_until_its_blank_line_and_flush_releases_the_tail", () => {
  const framer = new SseFramer();
  assert.deepEqual(framer.push("data: par"), []);
  assert.deepEqual(framer.push("tial\n"), []);
  assert.deepEqual(framer.flush(), ["partial"]);
  assert.deepEqual(framer.flush(), []);
});

test("sseFramer.splits_the_same_events_at_every_chunk_size", () => {
  const body = "event: delta\ndata: {\"a\":1}\n\nevent: delta\r\ndata: first\r\ndata: second\r\n\r\ndata: [DONE]\n\n";
  const expected = ['{"a":1}', "first\nsecond", "[DONE]"];
  for (const size of [1, 2, 3, 7, 4096]) {
    const framer = new SseFramer();
    const out: string[] = [];
    for (let i = 0; i < body.length; i += size) out.push(...framer.push(body.slice(i, i + size)));
    out.push(...framer.flush());
    assert.deepEqual(out, expected, `chunk size ${size}`);
  }
});

test("sseParser.capture_keeps_events_with_text_as_references_and_stops_at_its_cap", () => {
  const state = newSseState();
  const c = newStreamCapture(10_000);
  const events = [
    JSON.stringify({
      p: "",
      o: "add",
      v: {
        message: {
          id: "m1",
          author: { role: "assistant" },
          content: { content_type: "text", parts: ["He"] },
          recipient: "all",
        },
      },
    }),
    JSON.stringify({ p: "/message/content/parts/0", o: "append", v: "llo" }),
    JSON.stringify({ v: " wor" }),
    JSON.stringify({ p: "/message/status", o: "replace", v: "finished_successfully" }),
    "[DONE]",
    "not json, maybe a secret",
  ];
  for (const ev of events) captureEvent(state, ev, c);
  // The parser saw everything as usual.
  assert.equal(answerText(state), "Hello wor");
  assert.equal(isComplete(state), true);
  const out = renderCapture(c);
  assert.ok(out.includes('"parts":["«text 0+2»"]'), out);
  assert.ok(out.includes('{"p":"/message/content/parts/0","o":"append","v":"«text 2+3»"}'), out);
  assert.ok(out.includes('{"v":"«text 5+4»"}'), out);
  // Other strings stay whole, non-JSON events only by their length.
  assert.ok(out.includes('"v":"finished_successfully"'), out);
  assert.ok(out.includes("data: [DONE]\n"), out);
  assert.ok(out.includes("data: «24 chars»\n"), out);
  assert.ok(out.endsWith("above points here\nHello wor"), out);

  // Past the cap nothing more is kept, and the capture says so; the parser
  // still sees every event.
  const small = newStreamCapture(80);
  const st = newSseState();
  for (const ev of events) captureEvent(st, ev, small);
  assert.equal(answerText(st), "Hello wor");
  assert.ok(renderCapture(small).includes("capture full"), renderCapture(small));

  // A token whose rest did not fit is dropped from the kept text too.
  const cut = newStreamCapture(400);
  const s3 = newSseState();
  captureEvent(s3, events[0]!, cut);
  captureEvent(s3, JSON.stringify({ p: "/message/content/parts/0", o: "append", v: " key AKIAIOSF" }), cut);
  captureEvent(s3, JSON.stringify({ v: "ODNN7EXAMPLE".repeat(20) }), cut);
  assert.ok(cut.full);
  assert.equal(cut.pieces, "He key ");
});
