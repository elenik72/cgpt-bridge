// A browser start reaches the gateway window's record (TASK-054): Chrome
// restores the own window under new ids, and the record in
// chrome.storage.local notes the start so the window's page mark may take it
// back (targetTab.ts). The real background.ts on the fake `chrome`
// (harness/fakeChrome.ts); no browser.

import { before, mock, test } from "node:test";
import assert from "node:assert/strict";

// background.ts reads the `chrome` global when it loads; the fake goes first.
import { resetWorld, world } from "./harness/fakeChrome";
import { settle } from "./harness/clock";

before(async () => {
  // The bridge's keepalive interval must not outlive the test run.
  mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  mock.method(console, "log", () => {});
  mock.method(console, "warn", () => {});
  await import("../src/background");
});

test("startup.a_browser_start_is_noted_in_the_gateway_windows_record", async () => {
  resetWorld();
  world.local.ownWindowMark = { token: "t-1", windowId: 5, tabIds: [7, 8] };
  assert.equal(world.onStartup.length, 1, "background.ts listens to onStartup");
  world.onStartup[0]!();
  for (let i = 0; i < 10; i++) await settle();
  assert.deepEqual(world.local.ownWindowMark, {
    token: "t-1",
    windowId: 5,
    tabIds: [7, 8],
    browserStarted: true,
  });
});

test("startup.a_single_tab_record_of_an_earlier_version_is_noted_as_a_pool_of_one", async () => {
  resetWorld();
  world.local.ownWindowMark = { token: "t-1", windowId: 5, tabId: 7 };
  world.onStartup[0]!();
  for (let i = 0; i < 10; i++) await settle();
  assert.deepEqual(world.local.ownWindowMark, {
    token: "t-1",
    windowId: 5,
    tabIds: [7],
    browserStarted: true,
  });
});

test("startup.without_a_gateway_window_nothing_is_written", async () => {
  resetWorld();
  world.onStartup[0]!();
  for (let i = 0; i < 10; i++) await settle();
  assert.deepEqual(world.local, {});
});
