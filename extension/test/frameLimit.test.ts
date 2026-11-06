// The cap on what the background sends to the native host. The host drops a
// frame over its 1 MiB limit, so a larger answer has to become an error for
// that one ask before it is posted.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  HOST_FRAME_RESERVE_BYTES,
  MAX_HOST_FRAME_BYTES,
  WIRE_ERROR_CODES,
  askResultFrame,
  capHostFrame,
  errorFrame,
  hostFrameSize,
} from "../src/bridgeProtocol";

const BUDGET = MAX_HOST_FRAME_BYTES - HOST_FRAME_RESERVE_BYTES;

// An ask_result whose JSON is exactly `bytes` long.
function askResultOfSize(id: string, bytes: number) {
  const empty = hostFrameSize(askResultFrame(id, "", null));
  return askResultFrame(id, "a".repeat(bytes - empty), null);
}

test("frameLimit.small_answer_passes_unchanged", () => {
  const frame = askResultFrame("a1", "hello", "https://chatgpt.com/c/abc");
  assert.equal(capHostFrame(frame), frame);
});

test("frameLimit.limit_is_the_frame_limit_minus_a_reserve", () => {
  // Rust MAX_FRAME_BYTES (protocol/src/framing.rs).
  assert.equal(MAX_HOST_FRAME_BYTES, 1024 * 1024);
  assert.ok(HOST_FRAME_RESERVE_BYTES > 0);
});

test("frameLimit.answer_at_the_budget_passes_and_one_byte_more_is_capped", () => {
  const atBudget = askResultOfSize("a2", BUDGET);
  assert.equal(hostFrameSize(atBudget), BUDGET);
  assert.equal(capHostFrame(atBudget), atBudget);

  const over = askResultOfSize("a2", BUDGET + 1);
  assert.deepEqual(
    capHostFrame(over),
    errorFrame("a2", "oversize_frame", `ответ ${BUDGET + 1} байт больше лимита моста`),
  );
});

test("frameLimit.size_is_counted_in_utf8_bytes_not_characters", () => {
  // 600 000 Cyrillic letters are 600 000 characters but 1 200 000+ bytes.
  const frame = askResultFrame("a3", "я".repeat(600_000), null);
  const size = hostFrameSize(frame);
  assert.ok(size > MAX_HOST_FRAME_BYTES, `${size}`);
  assert.deepEqual(
    capHostFrame(frame),
    errorFrame("a3", "oversize_frame", `ответ ${size} байт больше лимита моста`),
  );
});

test("frameLimit.capped_frame_is_a_small_wire_error_with_the_same_id", () => {
  const capped = capHostFrame(askResultFrame("req_x", "z".repeat(2_000_000), null));
  assert.equal(capped.type, "error");
  assert.equal(capped.id, "req_x");
  if (capped.type !== "error") return;
  assert.ok(WIRE_ERROR_CODES.includes(capped.code));
  assert.ok(hostFrameSize(capped) < 1_000, `${hostFrameSize(capped)}`);
  // The answer text is not carried into the error.
  assert.ok(!JSON.stringify(capped).includes("zzzz"));
});

test("frameLimit.a_capture_that_does_not_fit_is_dropped_whole_and_never_fails_the_answer", () => {
  const check = { streamChars: 3, pageChars: 6, mismatch: true };
  // Text and capture together over the limit: the capture is left out, never
  // cut (its joined text sits at its end, where a cut could halve a secret);
  // the answer and the check stay whole, and the frame says why.
  const text = "t".repeat(300_000);
  const capture = 'data: {"v": "«text 0+1»"}\n\n'.repeat(30_000) + "AKIAIOSFODNN7EXAMPLE";
  const frame = askResultFrame("c1", text, null, null, { streamCheck: check, streamCapture: capture });
  assert.ok(hostFrameSize(frame) > BUDGET);
  const capped = capHostFrame(frame);
  assert.deepEqual(capped, {
    type: "ask_result",
    id: "c1",
    text,
    diagnostics: {
      stream_check: { stream_chars: 3, page_chars: 6, mismatch: true },
      stream_capture_dropped: true,
    },
  });
  assert.ok(hostFrameSize(capped) <= BUDGET);
  // The shape the Rust side reads (protocol message.rs
  // a_dropped_capture_still_parses): checked there with this same JSON.
  assert.equal(
    JSON.stringify(capHostFrame(askResultFrame("c9", "a".repeat(BUDGET - 200), null, null, { streamCapture: "x".repeat(10_000) }))).replace(/"text":"a+"/, '"text":"A"'),
    '{"type":"ask_result","id":"c9","text":"A","diagnostics":{"stream_capture_dropped":true}}',
  );

  // An answer over the limit by itself is still refused.
  const over = askResultFrame("c3", "a".repeat(BUDGET + 1), null, null, { streamCapture: "x" });
  assert.equal(capHostFrame(over).type, "error");
});
