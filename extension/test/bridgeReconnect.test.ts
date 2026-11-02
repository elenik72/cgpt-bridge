// The reconnect delay of the bridge to the native host, on the fake chrome and
// a mocked clock (nothing sleeps for real). The host connection is one
// long-lived module state, so the tests below run in order and build on each
// other: a host that cannot start, then one that starts and dies at once, then
// one that answers. Run with `npm run test` (esbuild bundle + node:test).

import { after, before, mock, test } from "node:test";
import assert from "node:assert/strict";

// bridge.ts reads the `chrome` global when it loads; the fake goes first.
import { world } from "./harness/fakeChrome";
import { startBridge } from "../src/bridge";
import { advance } from "./harness/clock";

const NOW = 1_700_000_000_000;
const STEP_MS = 5_000;
const MAX_DELAY_MS = 60_000;

before(() => {
  // The bridge logs every attempt, and a failed start with a stack trace.
  for (const level of ["log", "warn", "error"] as const) mock.method(console, level, () => {});
  mock.timers.enable({ apis: ["setTimeout", "setInterval", "Date"], now: NOW });
});
after(() => {
  mock.timers.reset();
  mock.restoreAll();
});

// Run the clock until `done()` holds (bounded).
async function until(done: () => boolean, maxMs = 200_000): Promise<void> {
  for (let waited = 0; !done() && waited < maxMs; waited += 100) await advance(100, 100);
  assert.ok(done(), "condition was not reached");
}
// Wait for the next native connection attempt.
const nextConnect = (): Promise<void> => {
  const seen = world.connects;
  return until(() => world.connects > seen);
};
// Gaps between consecutive connection attempts, rounded to the poll step.
const gaps = (): number[] =>
  world.connectTimes.slice(1).map((t, i) => Math.round((t - world.connectTimes[i]!) / 100) * 100);

test("a host that cannot start is retried with growing delays up to the maximum", async () => {
  world.connectThrows = true;
  startBridge();
  assert.equal(world.connects, 1, "the first attempt is immediate");
  while (world.connects < 15) await nextConnect();

  const g = gaps();
  assert.deepEqual(g.slice(0, 12), Array.from({ length: 12 }, (_, i) => STEP_MS * (i + 1)));
  assert.deepEqual(g.slice(12), [MAX_DELAY_MS, MAX_DELAY_MS], "capped at the maximum");
});

test("a connection sends a ping right away", async () => {
  world.connectThrows = false;
  await nextConnect();
  assert.equal(world.pings.length, 1);
  assert.equal(world.pings[0]?.type, "ping");
  assert.equal(typeof world.pings[0]?.id, "string");
});

test("a host that starts and dies without answering does not reset the delay", async () => {
  // Still at the maximum from the failed starts: connecting is not an answer.
  for (let i = 0; i < 2; i++) {
    const before = world.connectTimes.length;
    world.disconnect();
    await nextConnect();
    assert.equal(world.connectTimes.length, before + 1);
    assert.equal(gaps().at(-1), MAX_DELAY_MS);
  }
});

test("the first pong resets the delay", async () => {
  assert.ok(world.hostMessage, "the bridge opened its native port");
  world.hostMessage({ type: "pong", id: "p1" });
  world.disconnect();
  await nextConnect();
  assert.equal(gaps().at(-1), STEP_MS, "back to the first step");

  // No answer this time: the delays grow from the first step again.
  world.disconnect();
  await nextConnect();
  assert.equal(gaps().at(-1), 2 * STEP_MS);
});
