// Helpers for tests that run under `mock.timers` (setTimeout, setInterval and
// Date mocked): promise continuations still need real turns of the event loop.

import { AsyncResource } from "node:async_hooks";
import { after, mock } from "node:test";

// Let promise continuations run (setImmediate is not mocked).
export const settle = (): Promise<void> => new Promise((r) => setImmediate(r));

// Move the mocked clock forward in steps, letting the code under test react
// between steps (its next timer is only set after the previous one fired).
export async function advance(ms: number, step = 250): Promise<void> {
  await settle();
  for (let left = ms; left > 0; left -= step) {
    mock.timers.tick(Math.min(step, left));
    await settle();
  }
}

// ─── auto-advance ─────────────────────────────────────────────────────────

// Real turns the code under test gets to go quiet before the clock moves.
const QUIET_TURNS = 3;
// The clock moves at most this far per move, so work that is not on a timer
// (stream reads, promise chains) gets real turns over any long wait.
const MAX_STEP_MS = 50;

// The mocked timers' handles carry their due time (`runAt`).
interface MockTimer {
  runAt: number;
}

/**
 * Run the rest of this test file on a fake clock that moves by itself:
 * `mock.timers` (setTimeout, setInterval, Date) plus a driver that, whenever
 * the code under test has gone quiet, moves the clock to the next timer and
 * fires it — never past a timer, so timers fire in order at their due time
 * (those due at the same moment together), with promise continuations run
 * between them, as on a real clock. A test waits as written (`await
 * ask(...)`, `Date.now()` windows); its seconds take milliseconds. Call once
 * at the top of a file, after `loadPage()`; the clock is restored when the
 * file ends.
 */
export function useAutoClock(): void {
  mock.timers.enable({ apis: ["setTimeout", "setInterval", "Date"], now: Date.now() });

  // Track the pending timers to know the next one (mock.timers has no query).
  const pending = new Set<MockTimer>();
  type TimerApi = "setTimeout" | "setInterval" | "clearTimeout" | "clearInterval";
  const g = globalThis as unknown as Record<TimerApi, (...args: unknown[]) => unknown>;
  const fakeSetTimeout = g.setTimeout;
  const fakeSetInterval = g.setInterval;
  const fakeClearTimeout = g.clearTimeout;
  const fakeClearInterval = g.clearInterval;
  // A callback runs in the async context of the code that set it, as with a
  // real timer, not in the driver's: an error it throws belongs to that
  // test. It must not throw out of `mock.timers.tick` (the driver would die
  // and the file hang): it is rethrown on a real turn, as an uncaught
  // exception, as a real timer's is, and fails the test that set it.
  const guarded = (fn: unknown): ((...a: unknown[]) => void) =>
    AsyncResource.bind((...args: unknown[]) => {
      try {
        (fn as (...a: unknown[]) => void)(...args);
      } catch (err) {
        setImmediate(() => {
          throw err;
        });
      }
    });
  g.setTimeout = (fn: unknown, ms?: unknown, ...args: unknown[]) => {
    const call = guarded(fn);
    const timer = fakeSetTimeout(
      (...a: unknown[]) => {
        pending.delete(timer);
        call(...a);
      },
      ms,
      ...args,
    ) as MockTimer;
    pending.add(timer);
    return timer;
  };
  g.setInterval = (fn: unknown, ms?: unknown, ...args: unknown[]) => {
    const timer = fakeSetInterval(guarded(fn), ms, ...args) as MockTimer;
    pending.add(timer);
    return timer;
  };
  g.clearTimeout = (timer: unknown) => {
    pending.delete(timer as MockTimer);
    fakeClearTimeout(timer);
  };
  g.clearInterval = (timer: unknown) => {
    pending.delete(timer as MockTimer);
    fakeClearInterval(timer);
  };

  let driving = true;
  const driver = (async () => {
    while (driving) {
      for (let i = 0; i < QUIET_TURNS; i++) await settle();
      let next: MockTimer | null = null;
      for (const t of pending) if (!next || t.runAt < next.runAt) next = t;
      if (!next) continue;
      const gap = Math.max(0, next.runAt - Date.now());
      mock.timers.tick(Math.min(gap, MAX_STEP_MS));
    }
  })();

  after(async () => {
    driving = false;
    await driver;
    mock.timers.reset();
  });
}
