// A stopped turn goes to a host older than the `cancelled` code as `internal`
// with its message, and the gateway recognizes it by the exact text
// (gateway/src/backend.rs, USER_STOP_MESSAGES); its own test reads the same
// file. A reworded message fails here instead of turning a Stop back into a
// retried HTTP 500.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { CancelledError, CANCELLED_BY_STOP_MESSAGE, STOPPED_IN_PAGE_MESSAGE } from "../src/errors";

const [BY_STOP, IN_PAGE] = readFileSync(
  new URL("../../protocol/tests/fixtures/stop_messages.txt", import.meta.url),
  "utf8",
)
  .split("\n")
  .filter((line) => line !== "");

test("stop.messages_match_the_shared_fixture", () => {
  assert.equal(CANCELLED_BY_STOP_MESSAGE, BY_STOP);
  assert.equal(new CancelledError().message, BY_STOP);
  assert.equal(STOPPED_IN_PAGE_MESSAGE, IN_PAGE);
});

test("stop.bridge_sends_the_fixture_text", () => {
  // bridge.ts spells the message out where it answers a cancelled ask.
  const source = readFileSync(new URL("../src/bridge.ts", import.meta.url), "utf8");
  const messages = [...source.matchAll(/"cancelled",\s*"([^"]*)"/g)].map((m) => m[1]);
  assert.ok(messages.length >= 3, `found ${messages.length}`);
  for (const message of messages) assert.equal(message, BY_STOP);
});
