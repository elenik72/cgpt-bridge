// On install or update the background puts the page scripts into every
// ChatGPT tab that is already open (src/reinject.ts): Chrome injects the
// manifest's content scripts only into pages loaded later, and the scripts
// an earlier version left in a page no longer reach the extension. The real
// background.ts on the fake `chrome` (harness/fakeChrome.ts); no browser.

import { before, beforeEach, mock, test } from "node:test";
import assert from "node:assert/strict";

// background.ts reads the `chrome` global when it loads; the fake goes first.
import { resetWorld, world } from "./harness/fakeChrome";
import { settle } from "./harness/clock";
import { describeTab } from "../src/reinject";

const TABS = [
  { id: 3, url: "https://chatgpt.com/c/one", title: "One" },
  { id: 4, url: "https://chatgpt.com/", title: "ChatGPT", discarded: true },
  { id: 5, url: "https://chatgpt.com/g/x/c/two?model=a", title: "Two" },
];
let queries: chrome.tabs.QueryInfo[] = [];
let warnings: string[] = [];

before(async () => {
  // The bridge's keepalive interval must not outlive the test run.
  mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  mock.method(console, "log", () => {});
  mock.method(console, "info", () => {});
  mock.method(console, "warn", (...args: unknown[]) => void warnings.push(args.map(String).join(" ")));
  await import("../src/background");
});

beforeEach(() => {
  resetWorld();
  queries = [];
  warnings = [];
  (globalThis as unknown as { chrome: { tabs: unknown } }).chrome.tabs = {
    async query(q: chrome.tabs.QueryInfo) {
      queries.push(q);
      return TABS;
    },
  };
});

async function installed(reason: string): Promise<void> {
  assert.equal(world.onInstalled.length, 1, "background.ts listens to onInstalled");
  world.onInstalled[0]!({ reason });
  for (let i = 0; i < 10; i++) await settle();
}

for (const reason of ["install", "update"]) {
  test(`install.${reason}_injects_the_page_scripts_into_every_open_chatgpt_tab`, async () => {
    await installed(reason);

    assert.deepEqual(queries, [{ url: "https://chatgpt.com/*" }]);
    for (const tab of TABS) {
      assert.deepEqual(
        world.injected.filter((i) => i.tabId === tab.id),
        [
          { tabId: tab.id, file: "pageShim.js", world: "MAIN" },
          { tabId: tab.id, file: "content.js", world: "ISOLATED" },
        ],
      );
    }
    assert.deepEqual(warnings, []);
  });
}

test("install.a_tab_that_refuses_the_injection_is_one_log_line_and_the_others_still_get_it", async () => {
  world.inject = async (i) => {
    if (i.tabId === 4) throw new Error("Cannot access contents of the page. The tab was discarded.");
  };
  await installed("update");

  assert.equal(warnings.length, 1, warnings.join("\n"));
  assert.match(warnings[0]!, /tab 4/);
  assert.match(warnings[0]!, /The tab was discarded/);
  assert.deepEqual(
    world.injected.filter((i) => i.file === "content.js").map((i) => i.tabId),
    [3, 4, 5],
  );
});

test("install.a_shim_that_does_not_go_in_is_one_log_line_and_the_content_script_still_does", async () => {
  world.inject = async (i) => {
    if (i.tabId === 3 && i.file === "pageShim.js") throw new Error("Frame with ID 0 was removed.");
  };
  await installed("update");

  assert.equal(warnings.length, 1, warnings.join("\n"));
  assert.match(warnings[0]!, /tab 3: page shim not injected: Frame with ID 0 was removed/);
  assert.deepEqual(
    world.injected.filter((i) => i.file === "content.js").map((i) => i.tabId),
    [3, 4, 5],
  );
});

test("install.a_refusal_naming_an_address_is_logged_without_its_query", async () => {
  world.inject = async (i) => {
    if (i.tabId === 5) throw new Error('Cannot access contents of url "https://chatgpt.com/g/x/c/two?model=a#f".');
  };
  await installed("update");

  assert.equal(warnings.length, 1, warnings.join("\n"));
  assert.match(warnings[0]!, /url "https:\/\/chatgpt\.com\/g\/x\/c\/two"\./);
});

test("install.a_tab_is_described_by_a_short_title_and_its_address_without_query", () => {
  assert.equal(
    describeTab({ title: "Trip plans", url: "https://chatgpt.com/c/1?q=x#y" }),
    'the tab "Trip plans" at https://chatgpt.com/c/1',
  );
  const long = describeTab({ title: "t".repeat(200), url: "https://chatgpt.com/" });
  assert.equal(long, `the tab "${"t".repeat(59)}…" at https://chatgpt.com/`);
  assert.equal(describeTab({ title: "", url: "https://chatgpt.com/" }), "an untitled tab at https://chatgpt.com/");
  assert.equal(describeTab(null), "a tab that is gone");
});

test("install.a_browser_update_injects_nothing", async () => {
  await installed("chrome_update");
  assert.deepEqual(queries, []);
  assert.deepEqual(world.injected, []);
});
