// The auto-advancing clock (harness/clock.ts `useAutoClock`) must report an
// error thrown by a timer callback as a failing test, the way a real timer
// does, and keep moving for the tests after it. Before, `mock.timers.tick`
// rethrew it, the driver died and the file hung without a word.
//
// The checks run in a child process (this bundle again, in child mode) with
// a time limit, so a regression fails here instead of hanging `npm test`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import { useAutoClock } from "./harness/clock";

const CHILD = "CGPT_AUTO_CLOCK_CHILD";
const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

if (process.env[CHILD] === "1") {
  useAutoClock();

  test("child.timeout_callback_throws", async () => {
    setTimeout(() => {
      throw new Error("boom in a page timeout");
    }, 50);
    await sleep(100);
  });

  test("child.interval_callback_throws", async () => {
    const iv = setInterval(() => {
      clearInterval(iv);
      throw new Error("boom in a page interval");
    }, 50);
    await sleep(100);
  });

  // 10 s on the fake clock: only passes if the clock still moves by itself.
  test("child.clock_still_moves_after_the_errors", async () => {
    const started = Date.now();
    await sleep(10_000);
    assert.equal(Date.now() - started, 10_000);
  });
} else {
  test("autoClock.a_throwing_timer_callback_fails_its_test_instead_of_hanging", () => {
    // Run as a plain script: the test runner's child protocol stays off. The
    // reporter is pinned to TAP (the default differs between Node versions);
    // stderr (an ExperimentalWarning for MockTimers on Node 22) is not checked.
    const env: NodeJS.ProcessEnv = { ...process.env, [CHILD]: "1" };
    delete env.NODE_TEST_CONTEXT;
    const child = spawnSync(process.execPath, ["--test-reporter=tap", fileURLToPath(import.meta.url)], {
      env,
      encoding: "utf8",
      timeout: 10_000,
    });
    const tap = child.stdout;
    const out = `${tap}\n${child.stderr}`;
    assert.equal(child.signal, null, `the child hung and was killed (${child.signal}):\n${out}`);
    assert.notEqual(child.status, 0, out);
    assert.match(tap, /^not ok \d+ - child\.timeout_callback_throws$/m, out);
    assert.match(tap, /^not ok \d+ - child\.interval_callback_throws$/m, out);
    assert.match(tap, /^ok \d+ - child\.clock_still_moves_after_the_errors$/m, out);
    assert.match(tap, /boom in a page timeout/, out);
    assert.match(tap, /boom in a page interval/, out);
  });
}
