// Pure-logic tests for the gateway side of the extension: which tab a request
// goes to, how chat paths are read, and how the page shim reports HTTP 429.
// Run with `npm run test` (esbuild bundle + node:test, no browser).

import { test } from "node:test";
import assert from "node:assert/strict";

import { OWN_TABS_MAX, TabChooser, UNSETTLED_MS, type TabApi } from "../src/targetTab";
import { TabQueue } from "../src/askControl";
import type { ChatTarget } from "../src/bridgeProtocol";
import { conversationPathFrom, isRateLimitSignal } from "../src/chatgptAdapter";

// ─── fake chrome.* ──────────────────────────────────────────────────────

interface FakeTab {
  id: number;
  windowId: number;
  url: string;
  active: boolean;
  status?: string;
  windowType?: string;
  discarded?: boolean;
}

function fakeChrome(opts: {
  tabs: FakeTab[];
  focusedWindowId: number;
  session?: Record<string, unknown>;
  local?: Record<string, unknown>;
  // The first chrome.storage.local read waits for this.
  holdFirstLocalGet?: Promise<void>;
}) {
  let hold = opts.holdFirstLocalGet;
  const tabs = [...opts.tabs];
  const session: Record<string, unknown> = { ...(opts.session ?? {}) };
  const local: Record<string, unknown> = { ...(opts.local ?? {}) };
  const createdWindows: chrome.windows.CreateData[] = [];
  const createdTabs: chrome.tabs.CreateProperties[] = [];
  const updates: Array<[number, chrome.tabs.UpdateProperties]> = [];
  let nextId = 100;

  const api: TabApi = {
    tabs: {
      async get(tabId) {
        const tab = tabs.find((t) => t.id === tabId);
        if (!tab) throw new Error(`No tab with id: ${tabId}.`);
        return tab as unknown as chrome.tabs.Tab;
      },
      async query(q) {
        const prefix = typeof q.url === "string" ? q.url.replace(/\*$/, "") : null;
        return tabs.filter(
          (t) =>
            (q.active === undefined || t.active === q.active) &&
            (q.lastFocusedWindow !== true || t.windowId === opts.focusedWindowId) &&
            (q.windowType === undefined || (t.windowType ?? "normal") === q.windowType) &&
            (prefix === null || t.url.startsWith(prefix)),
        ) as unknown as chrome.tabs.Tab[];
      },
      async update(tabId, props) {
        updates.push([tabId, props]);
        return tabs.find((t) => t.id === tabId) as unknown as chrome.tabs.Tab;
      },
      async create(props) {
        createdTabs.push(props);
        if (!tabs.some((t) => t.windowId === props.windowId)) {
          throw new Error(`No window with id: ${props.windowId}.`);
        }
        const tab: FakeTab = {
          id: nextId++,
          windowId: props.windowId as number,
          url: String(props.url ?? ""),
          active: props.active ?? true,
          status: "loading",
        };
        tabs.push(tab);
        return tab as unknown as chrome.tabs.Tab;
      },
    },
    windows: {
      async create(data) {
        createdWindows.push(data ?? {});
        const windowId = nextId++;
        const tab: FakeTab = {
          id: nextId++,
          windowId,
          url: String(data?.url ?? ""),
          active: true,
          status: "loading",
        };
        tabs.push(tab);
        return { id: windowId, tabs: [tab] } as unknown as chrome.windows.Window;
      },
    },
    storage: {
      session: {
        async get(key) {
          return key in session ? { [key]: session[key] } : {};
        },
        async set(items) {
          Object.assign(session, items);
        },
      },
      local: {
        async get(key) {
          const snapshot = key in local ? { [key]: JSON.parse(JSON.stringify(local[key])) } : {};
          if (hold !== undefined) {
            const wait = hold;
            hold = undefined;
            await wait;
          }
          return snapshot;
        },
        async set(items) {
          Object.assign(local, JSON.parse(JSON.stringify(items)));
        },
        async remove(key) {
          delete local[key];
        },
      },
    },
  };
  return { api, tabs, session, local, createdWindows, createdTabs, updates };
}

// The page marks the content scripts report (TASK-054): tab id → the mark in
// that page's sessionStorage. A tab in `unreadable` does not answer.
function fakeMarks(marks: Record<number, string> = {}, unreadable: number[] = []) {
  const asked: number[] = [];
  const readMark = async (tabId: number): Promise<string | null | undefined> => {
    asked.push(tabId);
    if (unreadable.includes(tabId)) return undefined;
    return marks[tabId] ?? null;
  };
  return { readMark, asked, marks };
}

// A targeted ask that runs and ends before the next one: the reservation
// `tabForAsk` holds on the pool tab goes, as the background lets it go once
// the ask is in the tab's queue.
async function askOnce(chooser: TabChooser, target: ChatTarget): Promise<number> {
  const tabId = await chooser.tabForAsk(target);
  chooser.unreserve(tabId);
  return tabId;
}

// The single-tab session record of the version before the pool (T7): read
// as a pool of one.
const OWN = { windowId: 5, tabId: 7 };
const ownTab: FakeTab = { id: 7, windowId: 5, url: "https://chatgpt.com/c/own-chat", active: true };

// ─── targetTab ──────────────────────────────────────────────────────────

test("targetTab.chooses_own_tab_for_targeted_ask", async () => {
  const fake = fakeChrome({
    focusedWindowId: 1,
    session: { ownTab: OWN },
    tabs: [
      ownTab,
      { id: 3, windowId: 1, url: "https://chatgpt.com/c/user-chat", active: true },
    ],
  });
  const chooser = new TabChooser(fake.api);

  // One ask after the other: the one pool tab is free each time.
  assert.equal(await askOnce(chooser, { kind: "new_chat" }), 7);
  assert.equal(
    await askOnce(chooser, { kind: "conversation", url: "https://chatgpt.com/c/other" }),
    7,
  );
  assert.equal(fake.createdWindows.length, 0);
  assert.equal(fake.createdTabs.length, 0);
  // `status` reads the same tab.
  assert.equal((await chooser.ownTab())?.id, 7);
});

test("targetTab.untargeted_ask_uses_legacy_active_tab_rules", async () => {
  // The user's active ChatGPT tab in the focused window wins; the own tab is
  // never used for untargeted asks, even though it is active in its window.
  const fake = fakeChrome({
    focusedWindowId: 1,
    session: { ownTab: OWN },
    tabs: [
      ownTab,
      { id: 3, windowId: 1, url: "https://chatgpt.com/c/user-chat", active: true },
      { id: 4, windowId: 2, url: "https://chatgpt.com/c/older", active: false },
    ],
  });
  const chooser = new TabChooser(fake.api);
  assert.equal(await chooser.tabForAsk(undefined), 3);

  // The tab that answered last stays sticky while it shows the same chat.
  await chooser.rememberLegacyTarget(4);
  assert.equal(await chooser.tabForAsk(undefined), 4);
  // No own tab is opened for the CLI.
  assert.equal(fake.createdWindows.length, 0);

  // Focused window shows something else: an active ChatGPT tab in another
  // window is next — but not the own tab (window 5).
  const other = fakeChrome({
    focusedWindowId: 9,
    session: { ownTab: OWN },
    tabs: [ownTab, { id: 3, windowId: 1, url: "https://chatgpt.com/", active: true }],
  });
  assert.equal(await new TabChooser(other.api).tabForAsk(undefined), 3);

  // Only the own tab is open: refuse instead of writing into the gateway chat.
  const onlyOwn = fakeChrome({ focusedWindowId: 1, session: { ownTab: OWN }, tabs: [ownTab] });
  await assert.rejects(new TabChooser(onlyOwn.api).tabForAsk(undefined), /no https:\/\/chatgpt\.com\/ tab/);
});

// AUD-009: the CLI's sticky tab is a convenience for follow-ups, not a
// guarantee. When the remembered tab no longer shows the same chat the
// extension falls back to the active tab, so the CLI agent compares the
// `conversation_url` of every reply (see cli/tests/agent_chat.rs).
test("targetTab.sticky_tab_is_used_only_while_it_shows_the_same_chat", async () => {
  const fake = fakeChrome({
    focusedWindowId: 1,
    tabs: [
      { id: 3, windowId: 1, url: "https://chatgpt.com/c/user-chat", active: true },
      { id: 4, windowId: 2, url: "https://chatgpt.com/c/agent-chat", active: false },
    ],
  });
  const chooser = new TabChooser(fake.api);
  await chooser.rememberLegacyTarget(4);
  assert.equal(await chooser.tabForAsk(undefined), 4);

  // The remembered tab moves to another chat: the follow-up goes to the
  // active tab instead, and the sticky entry is gone for good.
  fake.tabs[1]!.url = "https://chatgpt.com/c/someone-elses-chat";
  assert.equal(await chooser.tabForAsk(undefined), 3);
  fake.tabs[1]!.url = "https://chatgpt.com/c/agent-chat";
  assert.equal(await chooser.tabForAsk(undefined), 3);

  // A closed tab is dropped the same way.
  await chooser.rememberLegacyTarget(3);
  fake.tabs.splice(0, 1);
  fake.tabs[0]!.active = true;
  assert.equal(await chooser.tabForAsk(undefined), 4);
});

test("targetTab.a_tab_without_a_saved_chat_is_not_remembered", async () => {
  // The answer left the tab on the temporary new-chat URL (no `/c/<id>`): no
  // chat to stay in, so the next ask picks the active tab as for a first ask.
  const fake = fakeChrome({
    focusedWindowId: 1,
    tabs: [
      { id: 3, windowId: 1, url: "https://chatgpt.com/c/user-chat", active: true },
      { id: 4, windowId: 2, url: "https://chatgpt.com/", active: false },
    ],
  });
  const chooser = new TabChooser(fake.api);
  await chooser.rememberLegacyTarget(4);
  assert.equal(await chooser.tabForAsk(undefined), 3);

  // A tab that is already gone cannot be remembered either.
  await chooser.rememberLegacyTarget(99);
  assert.equal(await chooser.tabForAsk(undefined), 3);
});

test("targetTab.reopens_own_window_when_closed", async () => {
  // Stored own tab (7) is gone; the user's tab must not be taken over.
  const fake = fakeChrome({
    focusedWindowId: 1,
    session: { ownTab: OWN },
    tabs: [{ id: 3, windowId: 1, url: "https://chatgpt.com/c/user-chat", active: true }],
  });
  const chooser = new TabChooser(fake.api);
  assert.equal(await chooser.ownTab(), null);

  const tabId = await chooser.tabForAsk({ kind: "new_chat" });
  assert.deepEqual(fake.createdWindows, [
    { url: "https://chatgpt.com/", focused: false, state: "normal" },
  ]);
  assert.notEqual(tabId, 3);
  assert.deepEqual(fake.session.ownTab, { windowId: 100, tabIds: [tabId] });
  // Memory Saver must not unload the long-lived gateway tab.
  assert.deepEqual(fake.updates, [[tabId, { autoDiscardable: false }]]);
  chooser.unreserve(tabId);

  // Next targeted ask reuses the new window.
  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), tabId);
  assert.equal(fake.createdWindows.length, 1);
});

// ─── own window after an extension reload or a browser restart (TASK-054) ──
//
// chrome.storage.session is empty after either. The own window is taken
// back only by the mark its page carries (written by the gateway's asks)
// matching the record in chrome.storage.local; any doubt leaves it an
// ordinary tab and the gateway opens a new window.

const TOKEN = "3f0c9a52-6d1e-4b7a-9c2f-0e8d5a4b1c77";
const MARK_RECORD = { token: TOKEN, windowId: 5, tabId: 7 };
// The lines the chooser logs when it takes a window back or refuses to.
function quiet(t: { mock: { method: (o: object, name: string, fn: () => void) => unknown } }): void {
  t.mock.method(console, "info", () => {});
  t.mock.method(console, "warn", () => {});
}
const userTab: FakeTab = { id: 3, windowId: 1, url: "https://chatgpt.com/c/user-chat", active: true };

test("targetTab.a_new_own_window_gets_a_mark_record_and_its_asks_carry_the_mark", async (t) => {
  quiet(t);
  const fake = fakeChrome({ focusedWindowId: 1, tabs: [userTab] });
  const chooser = new TabChooser(fake.api, fakeMarks().readMark);
  const tabId = await chooser.tabForAsk({ kind: "new_chat" });

  const rec = fake.local.ownWindowMark as { token: string; windowId: number; tabIds: number[] };
  assert.deepEqual(rec.tabIds, [tabId]);
  assert.equal(rec.windowId, 100);
  assert.match(rec.token, /^[0-9a-f-]{36}$/);
  assert.equal(await chooser.ownMarkFor(tabId), rec.token);
  // Never for another tab: the mark goes only into the own tab's page.
  assert.equal(await chooser.ownMarkFor(3), null);
  // Each new own window gets its own mark.
  const other = fakeChrome({ focusedWindowId: 1, tabs: [] });
  await new TabChooser(other.api).tabForAsk({ kind: "new_chat" });
  assert.notEqual((other.local.ownWindowMark as { token: string }).token, rec.token);
});

test("targetTab.after_a_reload_the_own_window_is_taken_back_by_its_mark", async (t) => {
  quiet(t);
  // The live case of 28.10: the own window is the last-focused one.
  const fake = fakeChrome({
    focusedWindowId: 5,
    local: { ownWindowMark: MARK_RECORD },
    tabs: [ownTab, { ...userTab, active: true }],
  });
  const marks = fakeMarks({ 7: TOKEN });
  const chooser = new TabChooser(fake.api, marks.readMark);

  // The CLI does not take it…
  assert.equal(await chooser.tabForAsk(undefined), 3);
  // …the gateway does, and opens no second window.
  assert.equal(await askOnce(chooser, { kind: "new_chat" }), 7);
  assert.equal(fake.createdWindows.length, 0);
  assert.deepEqual(fake.session.ownTab, { windowId: 5, tabIds: [7] });
  assert.deepEqual(fake.updates, [[7, { autoDiscardable: false }]]);
  assert.equal((await chooser.ownTab())?.id, 7);
  // Taken back once: no more questions to the pages.
  const asked = marks.asked.length;
  await chooser.tabForAsk(undefined);
  await askOnce(chooser, { kind: "new_chat" });
  assert.equal(marks.asked.length, asked);
});

test("targetTab.a_user_tab_without_the_mark_is_never_taken_for_the_own_window", async (t) => {
  quiet(t);
  // The record names tab 7, which is now the user's own tab (no mark, or
  // another one): it stays ordinary, the CLI may use it, the gateway opens
  // a new window.
  for (const mark of [undefined, "some-other-mark"]) {
    const fake = fakeChrome({
      focusedWindowId: 5,
      local: { ownWindowMark: MARK_RECORD },
      tabs: [ownTab],
    });
    const chooser = new TabChooser(fake.api, fakeMarks(mark === undefined ? {} : { 7: mark }).readMark);
    assert.equal(await chooser.ownTab(), null);
    assert.equal(await chooser.tabForAsk(undefined), 7);
    const tabId = await chooser.tabForAsk({ kind: "new_chat" });
    assert.notEqual(tabId, 7);
    assert.equal(fake.createdWindows.length, 1);
  }
});

test("targetTab.no_mark_record_means_no_page_is_asked", async (t) => {
  quiet(t);
  const fake = fakeChrome({ focusedWindowId: 5, tabs: [ownTab] });
  const marks = fakeMarks({ 7: TOKEN });
  const chooser = new TabChooser(fake.api, marks.readMark);
  assert.equal(await chooser.tabForAsk(undefined), 7);
  assert.deepEqual(marks.asked, []);
});

test("targetTab.after_a_reload_the_recorded_tab_is_the_only_one_asked", async (t) => {
  quiet(t);
  // The ids are proof enough with the mark: a tab the user opened next to it
  // changes nothing, and no other tab (here: a lone chatgpt.com tab with the
  // mark of a duplicate) is asked or taken.
  const fake = fakeChrome({
    focusedWindowId: 1,
    local: { ownWindowMark: MARK_RECORD },
    tabs: [
      ownTab,
      { id: 8, windowId: 5, url: "https://example.com/", active: false },
      { id: 9, windowId: 6, url: "https://chatgpt.com/c/own-chat", active: true },
      userTab,
    ],
  });
  const marks = fakeMarks({ 7: TOKEN, 9: TOKEN });
  const chooser = new TabChooser(fake.api, marks.readMark);
  assert.equal((await chooser.ownTab())?.id, 7);
  assert.deepEqual(marks.asked, [7]);
  assert.equal(await chooser.tabForAsk(undefined), 3);

  // A recorded id that now belongs to a tab in another window is not it.
  const moved = fakeChrome({
    focusedWindowId: 1,
    local: { ownWindowMark: { ...MARK_RECORD, windowId: 4 } },
    tabs: [ownTab],
  });
  assert.equal(await new TabChooser(moved.api, fakeMarks({ 7: TOKEN }).readMark).ownTab(), null);
});

test("targetTab.after_a_restart_two_lone_tabs_with_the_mark_leave_both_ordinary", async (t) => {
  quiet(t);
  // A duplicate of the own tab moved to a window of its own carries the
  // same mark (Chrome copies sessionStorage), and after a restart neither
  // has the recorded ids: no way to tell which is which.
  const fake = fakeChrome({
    focusedWindowId: 1,
    local: { ownWindowMark: MARK_RECORD },
    tabs: [
      { id: 12, windowId: 11, url: "https://chatgpt.com/c/own-chat", active: true },
      { id: 14, windowId: 13, url: "https://chatgpt.com/c/own-chat", active: true },
    ],
  });
  const chooser = new TabChooser(fake.api, fakeMarks({ 12: TOKEN, 14: TOKEN }).readMark);
  await chooser.noteBrowserStart();
  assert.equal(await chooser.ownTab(), null);
  assert.equal(fake.session.ownTab, undefined);
  // Both are ordinary tabs for the CLI now.
  assert.equal(await chooser.tabForAsk(undefined), 12);
});

test("targetTab.after_a_restart_a_marked_tab_sharing_its_window_or_in_a_popup_stays_ordinary", async (t) => {
  quiet(t);
  const restored: FakeTab = { id: 12, windowId: 11, url: "https://chatgpt.com/c/own-chat", active: true };
  const cases: FakeTab[][] = [
    [restored, { id: 13, windowId: 11, url: "https://example.com/", active: false }],
    [{ ...restored, windowType: "popup" }],
  ];
  for (const tabs of cases) {
    const fake = fakeChrome({ focusedWindowId: 1, local: { ownWindowMark: MARK_RECORD }, tabs });
    const marks = fakeMarks({ 12: TOKEN, 13: TOKEN });
    const chooser = new TabChooser(fake.api, marks.readMark);
    await chooser.noteBrowserStart();
    assert.equal(await chooser.ownTab(), null);
    assert.equal(fake.session.ownTab, undefined);
    assert.deepEqual(marks.asked, []);
  }
});

test("targetTab.after_a_restart_only_tabs_of_all_chatgpt_windows_are_asked_and_all_must_answer", async (t) => {
  quiet(t);
  // Tabs that share a window with tabs on other sites, and tabs on other
  // sites, are never asked; a window of ChatGPT tabs only (window 1) is.
  const restored: FakeTab = { id: 12, windowId: 11, url: "https://chatgpt.com/c/own-chat", active: true };
  const tabs: FakeTab[] = [
    restored,
    userTab,
    { id: 4, windowId: 1, url: "https://chatgpt.com/c/second", active: false },
    { id: 10, windowId: 2, url: "https://example.com/", active: true },
    { id: 15, windowId: 3, url: "https://chatgpt.com/c/lone-user-chat", active: true },
  ];
  const fake = fakeChrome({ focusedWindowId: 1, local: { ownWindowMark: MARK_RECORD }, tabs });
  const marks = fakeMarks({ 12: TOKEN });
  const chooser = new TabChooser(fake.api, marks.readMark);
  await chooser.noteBrowserStart();
  assert.equal((await chooser.ownTab())?.id, 12);
  assert.deepEqual([...marks.asked].sort((x, y) => x - y), [3, 4, 12, 15]);

  // A loaded lone tab that does not answer might carry the mark too:
  // nothing is taken. The scan runs once, so the record goes, and the window
  // stays ordinary once the tab can answer. (A tab Chrome has not loaded yet
  // is not waited for: see the pool tests.)
  for (const silent of [tabs[4]!]) {
    const doubt = fakeChrome({
      focusedWindowId: 1,
      local: { ownWindowMark: MARK_RECORD },
      tabs: [restored, silent],
    });
    const unreadable = silent.discarded ? [] : [15];
    const marks2 = fakeMarks({ 12: TOKEN }, unreadable);
    const c2 = new TabChooser(doubt.api, marks2.readMark);
    await c2.noteBrowserStart();
    assert.equal(await c2.ownTab(), null);
    assert.equal(doubt.local.ownWindowMark, undefined);
    unreadable.length = 0;
    doubt.tabs[1] = { ...silent, discarded: false };
    const asked = marks2.asked.length;
    assert.equal(await c2.ownTab(), null);
    assert.equal(await new TabChooser(doubt.api, marks2.readMark).ownTab(), null);
    assert.equal(marks2.asked.length, asked);
  }
});

test("targetTab.after_a_restart_a_copy_of_the_page_outliving_the_original_is_not_taken", async (t) => {
  quiet(t);
  // Chrome copies sessionStorage into a duplicated or reopened tab.
  const restored: FakeTab = { id: 12, windowId: 11, url: "https://chatgpt.com/c/own-chat", active: true };
  const copy: FakeTab = { id: 20, windowId: 19, url: "https://chatgpt.com/c/own-chat", active: true };

  // The restored window is taken back, then closed; the copy comes later.
  const fake = fakeChrome({ focusedWindowId: 1, local: { ownWindowMark: MARK_RECORD }, tabs: [restored] });
  const marks = fakeMarks({ 12: TOKEN, 20: TOKEN });
  const chooser = new TabChooser(fake.api, marks.readMark);
  await chooser.noteBrowserStart();
  assert.equal((await chooser.ownTab())?.id, 12);
  fake.tabs.splice(0, 1);
  assert.equal(await chooser.ownTab(), null);
  assert.equal(fake.local.ownWindowMark, undefined);
  fake.tabs.push(copy);
  // …then an extension reload: nothing in storage.session.
  delete fake.session.ownTab;
  const reloaded = new TabChooser(fake.api, marks.readMark);
  assert.equal(await reloaded.ownTab(), null);
  assert.equal(await reloaded.tabForAsk(undefined), 20);
  assert.notEqual(await reloaded.tabForAsk({ kind: "new_chat" }), 20);

  // The window was not restored: the one scan finds nothing and drops the
  // record; a copy reopened later in that session is not taken either.
  const gone = fakeChrome({ focusedWindowId: 1, local: { ownWindowMark: MARK_RECORD }, tabs: [userTab] });
  const c2 = new TabChooser(gone.api, marks.readMark);
  await c2.noteBrowserStart();
  assert.equal(await c2.ownTab(), null);
  assert.equal(gone.local.ownWindowMark, undefined);
  gone.tabs.push(copy);
  assert.equal(await c2.ownTab(), null);
  assert.equal(await new TabChooser(gone.api, marks.readMark).ownTab(), null);
});

test("targetTab.an_own_tab_that_left_chatgpt_is_not_used_but_stays_recorded", async (t) => {
  quiet(t);
  // A sign-in page, say: no request goes there, and back on chatgpt.com it
  // is the own tab again.
  const fake = fakeChrome({
    focusedWindowId: 1,
    session: { ownTab: OWN },
    local: { ownWindowMark: MARK_RECORD },
    tabs: [{ ...ownTab, url: "https://example.com/" }],
  });
  const chooser = new TabChooser(fake.api);
  assert.equal(await chooser.ownTab(), null);
  assert.deepEqual(fake.local.ownWindowMark, MARK_RECORD);
  fake.tabs[0]!.url = "https://chatgpt.com/c/own-chat";
  assert.equal((await chooser.ownTab())?.id, 7);
});

test("targetTab.the_cli_stays_off_the_recorded_tab_while_it_does_not_answer", async (t) => {
  quiet(t);
  const fake = fakeChrome({
    focusedWindowId: 5,
    local: { ownWindowMark: MARK_RECORD },
    tabs: [ownTab, { ...userTab, active: false }],
  });
  const unreadable = [7];
  const chooser = new TabChooser(fake.api, fakeMarks({ 7: TOKEN }, unreadable).readMark);
  // Tab 7 is the active tab of the focused window, yet the CLI gets tab 3.
  assert.equal(await chooser.tabForAsk(undefined), 3);
  fake.tabs.splice(1, 1);
  await assert.rejects(chooser.tabForAsk(undefined), /no https:\/\/chatgpt\.com\/ tab/);
  // Once it answers, it is the own tab.
  unreadable.length = 0;
  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), 7);
  assert.equal(fake.createdWindows.length, 0);
});

test("targetTab.a_browser_start_noted_while_a_new_window_opens_does_not_overwrite_its_record", async (t) => {
  quiet(t);
  // noteBrowserStart reads the old record first; meanwhile an ask finds no
  // window and opens one. The new window's record must be the one kept.
  let release!: () => void;
  const held = new Promise<void>((r) => (release = r));
  const fake = fakeChrome({
    focusedWindowId: 1,
    local: { ownWindowMark: MARK_RECORD },
    tabs: [userTab],
    holdFirstLocalGet: held,
  });
  const chooser = new TabChooser(fake.api, fakeMarks().readMark);
  const noted = chooser.noteBrowserStart();
  const asked = chooser.tabForAsk({ kind: "new_chat" });
  setTimeout(release, 10);
  const [, tabId] = await Promise.all([noted, asked]);
  const rec = fake.local.ownWindowMark as { token: string; tabIds: number[]; browserStarted?: boolean };
  assert.deepEqual(rec.tabIds, [tabId]);
  assert.notEqual(rec.token, TOKEN);
  assert.equal(rec.browserStarted, undefined);
  assert.equal(await chooser.ownMarkFor(tabId), rec.token);
});

test("targetTab.a_page_that_could_not_be_read_is_asked_again_next_time", async (t) => {
  quiet(t);
  const fake = fakeChrome({ focusedWindowId: 1, local: { ownWindowMark: MARK_RECORD }, tabs: [ownTab] });
  const unreadable = [7];
  const marks = fakeMarks({ 7: TOKEN }, unreadable);
  const chooser = new TabChooser(fake.api, marks.readMark);
  // Still loading after a restart, say: not taken back yet, not given up on.
  assert.equal(await chooser.ownTab(), null);
  unreadable.length = 0;
  assert.equal((await chooser.ownTab())?.id, 7);

  // A page that answered "no mark" is not asked again in this worker's life.
  const plain = fakeChrome({
    focusedWindowId: 1,
    local: { ownWindowMark: { ...MARK_RECORD, windowId: 1, tabId: 3 } },
    tabs: [userTab],
  });
  const none = fakeMarks();
  const c2 = new TabChooser(plain.api, none.readMark);
  assert.equal(await c2.ownTab(), null);
  assert.equal(await c2.ownTab(), null);
  assert.deepEqual(none.asked, [3]);
});

test("targetTab.after_a_browser_restart_the_marked_window_is_taken_back_with_its_new_ids", async (t) => {
  quiet(t);
  // Chrome restored the own window with its page (sessionStorage too) under
  // new window and tab ids.
  const restored: FakeTab = { id: 12, windowId: 11, url: "https://chatgpt.com/c/own-chat", active: true };
  const fresh = () =>
    fakeChrome({ focusedWindowId: 1, local: { ownWindowMark: MARK_RECORD }, tabs: [restored, userTab] });

  // Without a browser start (an extension reload keeps the ids) a marked tab
  // with other ids is not the own one: it stays ordinary.
  const noStart = fresh();
  assert.equal(await new TabChooser(noStart.api, fakeMarks({ 12: TOKEN }).readMark).ownTab(), null);

  // After `onStartup` it is taken back, and the record follows the new ids.
  const started = fresh();
  const chooser = new TabChooser(started.api, fakeMarks({ 12: TOKEN }).readMark);
  await chooser.noteBrowserStart();
  assert.equal((await chooser.ownTab())?.id, 12);
  assert.deepEqual(started.session.ownTab, { windowId: 11, tabIds: [12] });
  assert.deepEqual(started.local.ownWindowMark, { token: TOKEN, windowId: 11, tabIds: [12] });
  assert.equal(await chooser.tabForAsk(undefined), 3);
  assert.equal(await chooser.ownMarkFor(12), TOKEN);

  // The start is kept in the record for a service worker that starts later
  // (a new TabChooser), until the window is taken back or replaced.
  const later = fresh();
  await new TabChooser(later.api).noteBrowserStart();
  assert.equal((later.local.ownWindowMark as { browserStarted?: boolean }).browserStarted, true);
  assert.equal((await new TabChooser(later.api, fakeMarks({ 12: TOKEN }).readMark).ownTab())?.id, 12);
  assert.equal((later.local.ownWindowMark as { browserStarted?: boolean }).browserStarted, undefined);

  // The own window was not restored: nothing is taken, a new one opens.
  const gone = fakeChrome({ focusedWindowId: 1, local: { ownWindowMark: MARK_RECORD }, tabs: [userTab] });
  const c3 = new TabChooser(gone.api, fakeMarks().readMark);
  await c3.noteBrowserStart();
  assert.notEqual(await c3.tabForAsk({ kind: "new_chat" }), 3);
  assert.equal(gone.createdWindows.length, 1);
  assert.equal((gone.local.ownWindowMark as { browserStarted?: boolean }).browserStarted, undefined);
});

// ─── the own window's tab pool (T7) ────────────────────────────────────
//
// Load = asks running or queued in a tab (the background's TabQueue), given
// to the chooser as a function; a tab handed out by `tabForAsk` counts as
// busy until `unreserve`.

const POOL_TOKEN = "9d2b7c1e-4f3a-4e8b-a1c6-5b0e7d9f2a34";
const poolTab = (id: number, chat: string): FakeTab => ({
  id,
  windowId: 5,
  url: `https://chatgpt.com/c/${chat}`,
  active: false,
});
function poolChrome(pool: FakeTab[], extra: FakeTab[] = []) {
  const tabIds = pool.map((t) => t.id);
  return fakeChrome({
    focusedWindowId: 1,
    session: { ownTab: { windowId: 5, tabIds } },
    local: { ownWindowMark: { token: POOL_TOKEN, windowId: 5, tabIds } },
    tabs: [...pool, ...extra],
  });
}
const conversation = (chat: string): ChatTarget => ({
  kind: "conversation",
  url: `https://chatgpt.com/c/${chat}`,
});

test("pool.a_free_tab_that_shows_the_chat_comes_first_then_any_free_tab", async () => {
  const fake = poolChrome([poolTab(7, "a"), poolTab(8, "b")]);
  const load = new Map<number, number>();
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, (id) => load.get(id) ?? 0);

  assert.equal(await askOnce(chooser, conversation("b")), 8);
  assert.equal(await askOnce(chooser, conversation("a")), 7);
  // A new chat, or a chat no tab shows: the first free tab.
  assert.equal(await askOnce(chooser, { kind: "new_chat" }), 7);
  assert.equal(await askOnce(chooser, conversation("c")), 7);
  // The tab with the chat is busy: another free tab, not a wait.
  load.set(8, 1);
  assert.equal(await askOnce(chooser, conversation("b")), 7);
  assert.equal(fake.createdTabs.length, 0);
  assert.equal(fake.createdWindows.length, 0);
});

test("pool.with_every_tab_busy_a_new_background_tab_opens_in_the_own_window", async (t) => {
  quiet(t);
  const fake = poolChrome([poolTab(7, "a"), poolTab(8, "b")]);
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, () => 0);
  const first = await chooser.tabForAsk(conversation("a"));
  const second = await chooser.tabForAsk(conversation("a"));
  const third = await chooser.tabForAsk(conversation("a"));

  assert.deepEqual([first, second], [7, 8]);
  assert.ok(![7, 8].includes(third));
  assert.deepEqual(fake.createdTabs, [
    { windowId: 5, url: "https://chatgpt.com/", active: false },
  ]);
  assert.equal(fake.createdWindows.length, 0);
  // Both records know it, the mark goes into its page, Memory Saver leaves it.
  assert.deepEqual(fake.session.ownTab, { windowId: 5, tabIds: [7, 8, third] });
  assert.deepEqual(fake.local.ownWindowMark, {
    token: POOL_TOKEN,
    windowId: 5,
    tabIds: [7, 8, third],
  });
  assert.equal(await chooser.ownMarkFor(third), POOL_TOKEN);
  assert.equal(await chooser.ownMarkFor(8), POOL_TOKEN);
  assert.deepEqual(fake.updates, [[third, { autoDiscardable: false }]]);
});

test("pool.at_eight_tabs_the_ask_waits_in_the_least_loaded_tab", async () => {
  assert.equal(OWN_TABS_MAX, 8);
  const pool = [7, 8, 9, 10, 11, 12, 13, 14].map((id) => poolTab(id, `chat-${id}`));
  const fake = poolChrome(pool);
  const load = new Map<number, number>([
    [7, 3],
    [8, 2],
    [9, 2],
    [10, 2],
    [11, 3],
    [12, 2],
    [13, 4],
    [14, 2],
  ]);
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, (id) => load.get(id) ?? 0);

  // Several are least loaded: the one that shows the chat, else the first.
  assert.equal(await askOnce(chooser, conversation("chat-12")), 12);
  assert.equal(await askOnce(chooser, { kind: "new_chat" }), 8);
  // A busier tab that shows the chat loses to a less loaded one.
  assert.equal(await askOnce(chooser, conversation("chat-13")), 8);
  // The asks given out but not queued yet count too.
  const a = await chooser.tabForAsk({ kind: "new_chat" });
  const b = await chooser.tabForAsk({ kind: "new_chat" });
  assert.deepEqual([a, b], [8, 9]);
  assert.equal(fake.createdTabs.length, 0, "no ninth tab");
});

test("pool.three_asks_at_once_get_three_tabs_of_one_new_window", async (t) => {
  quiet(t);
  const fake = fakeChrome({ focusedWindowId: 1, tabs: [userTab] });
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, () => 0);
  const got = await Promise.all([
    chooser.tabForAsk({ kind: "new_chat" }),
    chooser.tabForAsk(conversation("x")),
    chooser.tabForAsk({ kind: "new_chat" }),
  ]);

  assert.equal(new Set(got).size, 3, `distinct tabs: ${got.join(", ")}`);
  assert.ok(!got.includes(3), "never the user's tab");
  assert.equal(fake.createdWindows.length, 1);
  assert.equal(fake.createdTabs.length, 2);
  assert.ok(fake.createdTabs.every((c) => c.windowId === 100 && c.active === false));
  assert.deepEqual(fake.session.ownTab, { windowId: 100, tabIds: got });
  const rec = fake.local.ownWindowMark as { token: string; tabIds: number[] };
  assert.deepEqual(rec.tabIds, got);

  // The same with a pool of one free tab: it goes to one of them only.
  const one = poolChrome([poolTab(7, "a")]);
  const c2 = new TabChooser(one.api, fakeMarks().readMark, () => 0);
  const got2 = await Promise.all([1, 2, 3].map(() => c2.tabForAsk(conversation("a"))));
  assert.equal(new Set(got2).size, 3);
  assert.equal(got2.filter((id) => id === 7).length, 1);
  assert.equal(one.createdTabs.length, 2);
  assert.equal(one.createdWindows.length, 0);
});

test("pool.a_tab_that_cannot_be_opened_leaves_the_ask_to_the_least_loaded_tab", async (t) => {
  quiet(t);
  const fake = poolChrome([poolTab(7, "a"), poolTab(8, "b")]);
  fake.api.tabs.create = async () => {
    throw new Error("No window with id: 5.");
  };
  const load = new Map<number, number>([
    [7, 2],
    [8, 1],
  ]);
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, (id) => load.get(id) ?? 0);
  assert.equal(await chooser.tabForAsk(conversation("a")), 8);
  assert.equal(fake.createdWindows.length, 0);
});

test("pool.the_cli_never_gets_a_pool_tab", async () => {
  // Pool tab 8 is the active tab of the last-focused window.
  const fake = poolChrome(
    [poolTab(7, "a"), { ...poolTab(8, "b"), windowId: 5, active: true }],
    [{ id: 3, windowId: 2, url: "https://chatgpt.com/c/user-chat", active: false }],
  );
  const own = fakeChrome({
    focusedWindowId: 5,
    session: fake.session,
    local: fake.local,
    tabs: fake.tabs,
  });
  const chooser = new TabChooser(own.api);
  assert.equal(await chooser.tabForAsk(undefined), 3);
  // Remembered as sticky, a pool tab is still not taken.
  await chooser.rememberLegacyTarget(8);
  assert.equal(await chooser.tabForAsk(undefined), 3);
  // Only pool tabs open: refused.
  own.tabs.splice(2, 1);
  await assert.rejects(chooser.tabForAsk(undefined), /no https:\/\/chatgpt\.com\/ tab/);
});

test("pool.a_closed_or_moved_tab_leaves_the_pool_and_the_window_stays", async (t) => {
  quiet(t);
  const fake = poolChrome([poolTab(7, "a"), poolTab(8, "b"), poolTab(10, "d")]);
  fake.tabs.splice(0, 1); // 7 closed
  fake.tabs[0]!.windowId = 1; // 8 dragged into the user's window
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, () => 0);

  assert.deepEqual((await chooser.ownTabs()).map((tab) => tab.id), [10]);
  assert.deepEqual(fake.session.ownTab, { windowId: 5, tabIds: [10] });
  assert.deepEqual(fake.local.ownWindowMark, { token: POOL_TOKEN, windowId: 5, tabIds: [10] });
  assert.equal(await chooser.ownMarkFor(8), null);
  // The tab that left the pool is the user's now.
  assert.equal(await chooser.tabForAsk(undefined), 8);
  assert.equal(await askOnce(chooser, conversation("b")), 10);

  // The last one goes: the mark record goes, the next ask opens a window.
  fake.tabs.splice(fake.tabs.findIndex((tab) => tab.id === 10), 1);
  assert.deepEqual(await chooser.ownTabs(), []);
  assert.equal(fake.local.ownWindowMark, undefined);
  const fresh = await chooser.tabForAsk({ kind: "new_chat" });
  assert.equal(fake.createdWindows.length, 1);
  assert.deepEqual(fake.session.ownTab, { windowId: 100, tabIds: [fresh] });
});

test("pool.records_of_the_single_tab_version_are_read_as_a_pool_of_one", async (t) => {
  quiet(t);
  // Session record and mark record as the version before the pool wrote them.
  const fake = fakeChrome({
    focusedWindowId: 1,
    session: { ownTab: OWN },
    local: { ownWindowMark: MARK_RECORD },
    tabs: [ownTab, userTab],
  });
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, () => 0);
  assert.deepEqual((await chooser.ownTabs()).map((tab) => tab.id), [7]);
  assert.equal(await chooser.ownMarkFor(7), TOKEN);
  assert.equal(await chooser.tabForAsk(undefined), 3);
  // A second tab: both records in the new shape, the window and mark kept.
  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), 7);
  const added = await chooser.tabForAsk({ kind: "new_chat" });
  assert.deepEqual(fake.session.ownTab, { windowId: 5, tabIds: [7, added] });
  assert.deepEqual(fake.local.ownWindowMark, { token: TOKEN, windowId: 5, tabIds: [7, added] });

  // After an extension reload only the old mark record is left: taken back.
  const reloaded = fakeChrome({
    focusedWindowId: 1,
    local: { ownWindowMark: MARK_RECORD },
    tabs: [ownTab, userTab],
  });
  const c2 = new TabChooser(reloaded.api, fakeMarks({ 7: TOKEN }).readMark);
  assert.equal((await c2.ownTab())?.id, 7);
  assert.deepEqual(reloaded.session.ownTab, { windowId: 5, tabIds: [7] });
  assert.deepEqual(reloaded.local.ownWindowMark, { token: TOKEN, windowId: 5, tabIds: [7] });
});

test("pool.after_a_reload_the_recorded_tabs_of_the_proven_window_are_taken_back", async (t) => {
  quiet(t);
  // 7 carries the mark and proves the window; 8 never got an ask (no mark
  // yet) and 10 does not answer (loading): both are ours by their ids. 9
  // carries another window's mark, 11 is in the window but not recorded, 12
  // is recorded but now in another window: none of those.
  const local = { ownWindowMark: { token: POOL_TOKEN, windowId: 5, tabIds: [7, 8, 9, 10, 12] } };
  const tabs = [
    poolTab(7, "a"),
    poolTab(8, "b"),
    poolTab(9, "c"),
    poolTab(10, "d"),
    poolTab(11, "e"),
    { ...poolTab(12, "f"), windowId: 6 },
    userTab,
  ];
  const fake = fakeChrome({ focusedWindowId: 1, local, tabs });
  const marks = fakeMarks({ 7: POOL_TOKEN, 9: "another-window-mark" }, [10]);
  const chooser = new TabChooser(fake.api, marks.readMark, () => 0);

  assert.deepEqual((await chooser.ownTabs()).map((tab) => tab.id), [7, 8, 10]);
  assert.deepEqual([...marks.asked].sort((x, y) => x - y), [7, 8, 9, 10]);
  assert.deepEqual(fake.session.ownTab, { windowId: 5, tabIds: [7, 8, 10] });
  assert.deepEqual(fake.local.ownWindowMark, { token: POOL_TOKEN, windowId: 5, tabIds: [7, 8, 10] });
  assert.deepEqual(
    fake.updates.map(([id]) => id),
    [7, 8, 10],
  );
  assert.equal(await chooser.ownMarkFor(8), POOL_TOKEN);
  assert.equal(await chooser.ownMarkFor(9), null);
});

test("pool.after_a_reload_no_marked_tab_means_doubt_or_no_window", async (t) => {
  quiet(t);
  const local = { ownWindowMark: { token: POOL_TOKEN, windowId: 5, tabIds: [7, 8] } };
  const tabs = [poolTab(7, "a"), poolTab(8, "b")];

  // One does not answer yet: in doubt, the CLI stays off both; asked again.
  const doubt = fakeChrome({ focusedWindowId: 1, local, tabs: [...tabs, userTab] });
  const unreadable = [8];
  const c1 = new TabChooser(doubt.api, fakeMarks({ 8: POOL_TOKEN }, unreadable).readMark);
  assert.deepEqual(await c1.ownTabs(), []);
  assert.equal(await c1.tabForAsk(undefined), 3);
  doubt.tabs.splice(2, 1);
  await assert.rejects(c1.tabForAsk(undefined), /no https:\/\/chatgpt\.com\/ tab/);
  unreadable.length = 0;
  assert.deepEqual((await c1.ownTabs()).map((tab) => tab.id), [7, 8]);

  // All answer, none has the mark: the record goes, both are ordinary.
  const none = fakeChrome({ focusedWindowId: 1, local, tabs });
  const c2 = new TabChooser(none.api, fakeMarks().readMark);
  assert.deepEqual(await c2.ownTabs(), []);
  assert.equal(none.local.ownWindowMark, undefined);
  await assert.rejects(c2.tabForAsk(undefined), /2 chatgpt\.com tabs are open/);
});

test("pool.after_a_browser_start_only_the_marked_tabs_of_the_marked_window_are_taken_back", async (t) => {
  quiet(t);
  // Restored under new ids: window 11 with 12 (marked), 13 (no mark: a tab
  // the user opened there, say) and 14 (does not answer). The user's mixed
  // window is not asked.
  const restored: FakeTab[] = [12, 13, 14].map((id) => ({
    id,
    windowId: 11,
    url: `https://chatgpt.com/c/own-${id}`,
    active: id === 12,
  }));
  const tabs: FakeTab[] = [
    ...restored,
    userTab,
    { id: 4, windowId: 1, url: "https://example.com/", active: false },
    { id: 15, windowId: 3, url: "https://chatgpt.com/c/lone-user-chat", active: true },
  ];
  const local = { ownWindowMark: { token: POOL_TOKEN, windowId: 5, tabIds: [7, 8] } };
  const fake = fakeChrome({ focusedWindowId: 1, local, tabs });
  const marks = fakeMarks({ 12: POOL_TOKEN }, [14]);
  const chooser = new TabChooser(fake.api, marks.readMark, () => 0);
  await chooser.noteBrowserStart();

  assert.deepEqual((await chooser.ownTabs()).map((tab) => tab.id), [12]);
  assert.deepEqual([...marks.asked].sort((x, y) => x - y), [12, 13, 14, 15]);
  assert.deepEqual(fake.session.ownTab, { windowId: 11, tabIds: [12] });
  assert.deepEqual(fake.local.ownWindowMark, { token: POOL_TOKEN, windowId: 11, tabIds: [12] });
  assert.equal(await chooser.ownMarkFor(13), null);
  // 13 and 14 are not the pool's, nor the CLI's: they are in the own window.
  assert.equal(await chooser.tabForAsk(undefined), 3);
  for (const id of [3, 15]) fake.tabs.splice(fake.tabs.findIndex((tab) => tab.id === id), 1);
  await assert.rejects(chooser.tabForAsk(undefined), /no https:\/\/chatgpt\.com\/ tab/);
  // The next gateway ask goes to the marked tab, then to a new one.
  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), 12);
  const added = await chooser.tabForAsk({ kind: "new_chat" });
  assert.ok(![12, 13, 14].includes(added));
});

test("pool.after_a_browser_start_unloaded_tabs_of_the_marked_window_are_loaded_and_join_with_the_mark", async (t) => {
  quiet(t);
  // Restored: 12 active and loaded, 13 and 14 (marked) and 16 (no mark) not
  // loaded yet.
  const tabs: FakeTab[] = [
    { id: 12, windowId: 11, url: "https://chatgpt.com/c/own-12", active: true },
    { id: 13, windowId: 11, url: "https://chatgpt.com/c/own-13", active: false, status: "unloaded" },
    { id: 14, windowId: 11, url: "https://chatgpt.com/c/own-14", active: false, discarded: true },
    { id: 16, windowId: 11, url: "https://chatgpt.com/c/user-16", active: false, status: "unloaded" },
  ];
  const local = { ownWindowMark: { token: POOL_TOKEN, windowId: 5, tabIds: [7] } };
  const fake = fakeChrome({ focusedWindowId: 1, local, tabs });
  const reloads: number[] = [];
  fake.api.tabs.reload = async (tabId) => {
    reloads.push(tabId);
    const tab = fake.tabs.find((x) => x.id === tabId)!;
    tab.status = "complete";
    tab.discarded = false;
  };
  const marks = fakeMarks({ 12: POOL_TOKEN, 13: POOL_TOKEN, 14: POOL_TOKEN });
  const chooser = new TabChooser(fake.api, marks.readMark, () => 0);
  await chooser.noteBrowserStart();

  // The ask that brings the takeover gets the loaded tab at once.
  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), 12);
  await chooser.whenRestored();
  assert.deepEqual(reloads.sort((x, y) => x - y), [13, 14, 16]);
  assert.deepEqual(fake.session.ownTab, { windowId: 11, tabIds: [12, 13, 14] });
  assert.deepEqual(fake.local.ownWindowMark, { token: POOL_TOKEN, windowId: 11, tabIds: [12, 13, 14] });
  assert.ok(fake.updates.some(([id, p]) => id === 13 && p.autoDiscardable === false));
  assert.equal(await chooser.ownMarkFor(16), null);
  // Once per tab: a later look loads nothing again.
  await chooser.ownTabs();
  await chooser.whenRestored();
  assert.equal(reloads.length, 3);
});

test("pool.after_a_browser_start_unloaded_chatgpt_tabs_elsewhere_do_not_block_the_takeover", async (t) => {
  quiet(t);
  // Chrome restores background tabs lazily: the user's window of ChatGPT
  // tabs 20 and 21 is not loaded yet (and cannot be asked).
  const tabs: FakeTab[] = [
    { id: 12, windowId: 11, url: "https://chatgpt.com/c/own-12", active: true },
    { id: 20, windowId: 19, url: "https://chatgpt.com/c/user-1", active: true, discarded: true },
    { id: 21, windowId: 19, url: "https://chatgpt.com/c/user-2", active: false, status: "unloaded" },
  ];
  const local = { ownWindowMark: { token: POOL_TOKEN, windowId: 5, tabIds: [7] } };
  const fake = fakeChrome({ focusedWindowId: 1, local, tabs });
  const marks = fakeMarks({ 12: POOL_TOKEN });
  const chooser = new TabChooser(fake.api, marks.readMark);
  await chooser.noteBrowserStart();
  assert.deepEqual((await chooser.ownTabs()).map((tab) => tab.id), [12]);
  assert.deepEqual(marks.asked, [12], "unloaded tabs are not asked");

  // A loaded one there that does not answer still blocks it.
  const loaded = fakeChrome({
    focusedWindowId: 1,
    local,
    tabs: [tabs[0]!, { ...tabs[1]!, discarded: false }, tabs[2]!],
  });
  const c2 = new TabChooser(loaded.api, fakeMarks({ 12: POOL_TOKEN }, [20]).readMark);
  await c2.noteBrowserStart();
  assert.deepEqual(await c2.ownTabs(), []);
  assert.equal(loaded.local.ownWindowMark, undefined);
});

test("pool.a_tab_on_a_sign_in_page_gets_no_request_and_is_never_the_clis", async (t) => {
  quiet(t);
  const fake = poolChrome(
    [poolTab(7, "a"), poolTab(8, "b")],
    [{ id: 3, windowId: 2, url: "https://chatgpt.com/c/user-chat", active: false }],
  );
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, () => 0);
  // 7 is redirected to the sign-in page.
  fake.tabs[0]!.url = "https://auth.openai.com/log-in";
  assert.deepEqual((await chooser.ownTabs()).map((tab) => tab.id), [8]);
  assert.equal(await askOnce(chooser, conversation("a")), 8);
  assert.equal(await chooser.tabForAsk(undefined), 3);
  // Still in the pool's records.
  assert.deepEqual(fake.session.ownTab, { windowId: 5, tabIds: [7, 8] });
  assert.equal(await chooser.ownMarkFor(7), POOL_TOKEN);

  // Back on chatgpt.com: the pool's again, and still not the CLI's.
  fake.tabs[0]!.url = "https://chatgpt.com/c/a";
  fake.tabs[0]!.active = true;
  assert.deepEqual((await chooser.ownTabs()).map((tab) => tab.id), [7, 8]);
  assert.equal(await askOnce(chooser, conversation("a")), 7);
  fake.tabs.splice(2, 1);
  await assert.rejects(chooser.tabForAsk(undefined), /no https:\/\/chatgpt\.com\/ tab/);

  // Every pool tab away and the pool full: refused, no ninth tab.
  const full = poolChrome([7, 8, 9, 10, 11, 12, 13, 14].map((id) => ({
    ...poolTab(id, "x"),
    url: "https://auth.openai.com/log-in",
  })));
  const c2 = new TabChooser(full.api, fakeMarks().readMark, () => 0);
  await assert.rejects(c2.tabForAsk({ kind: "new_chat" }), /sign in to ChatGPT there/);
  assert.equal(full.createdTabs.length, 0);
});

test("pool.the_cli_never_gets_any_tab_of_the_own_window", async () => {
  // 9 is in the own window but not in the pool (the user opened it there).
  const fake = poolChrome(
    [poolTab(7, "a")],
    [{ id: 9, windowId: 5, url: "https://chatgpt.com/c/user-chat", active: true }],
  );
  const chooser = new TabChooser(fake.api);
  await assert.rejects(chooser.tabForAsk(undefined), /no https:\/\/chatgpt\.com\/ tab/);
});

test("pool.a_new_tab_that_cannot_be_recorded_is_closed_and_the_ask_waits_in_a_pool_tab", async (t) => {
  quiet(t);
  const fake = poolChrome([poolTab(7, "a"), poolTab(8, "b")]);
  const removed: number[] = [];
  fake.api.tabs.remove = async (tabId) => {
    removed.push(tabId);
    fake.tabs.splice(fake.tabs.findIndex((tab) => tab.id === tabId), 1);
  };
  const realSet = fake.api.storage.local.set;
  fake.api.storage.local.set = async () => {
    throw new Error("QUOTA_BYTES quota exceeded");
  };
  const load = new Map<number, number>([
    [7, 2],
    [8, 1],
  ]);
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, (id) => load.get(id) ?? 0);
  assert.equal(await chooser.tabForAsk(conversation("a")), 8);
  assert.equal(fake.createdTabs.length, 1, "opened once, not again in the same choice");
  assert.equal(removed.length, 1);
  assert.ok(!fake.tabs.some((tab) => tab.id === removed[0]));
  assert.deepEqual(fake.session.ownTab, { windowId: 5, tabIds: [7, 8] });
  fake.api.storage.local.set = realSet;
});

test("pool.a_free_tab_whose_last_ask_ended_unsettled_comes_after_the_other_free_tabs", async () => {
  const fake = poolChrome([poolTab(7, "a"), poolTab(8, "b"), poolTab(9, "c")]);
  let now = 1_000_000;
  const c = new TabChooser(fake.api, fakeMarks().readMark, () => 0, () => now);
  for (const code of ["timeout", "cancelled", "tab_busy"]) {
    c.noteOutcome(7, code);
    // A new chat, or a chat no tab shows: another free tab first.
    assert.equal(await askOnce(c, { kind: "new_chat" }), 8, code);
    assert.equal(await askOnce(c, conversation("x")), 8, code);
  }
  // Other errors and answers leave it as any free tab.
  c.noteOutcome(7, "dom_failure");
  assert.equal(await askOnce(c, { kind: "new_chat" }), 7);
  c.noteOutcome(7, "timeout");
  c.noteOutcome(7, null);
  assert.equal(await askOnce(c, { kind: "new_chat" }), 7);

  // Nothing else free: still used, before a new tab is opened.
  c.noteOutcome(7, "timeout");
  c.noteOutcome(8, "cancelled");
  c.noteOutcome(9, "tab_busy");
  assert.equal(await askOnce(c, { kind: "new_chat" }), 7);
  assert.equal(fake.createdTabs.length, 0);

  // After UNSETTLED_MS it is any free tab again.
  c.noteOutcome(8, null);
  c.noteOutcome(9, null);
  assert.equal(await askOnce(c, { kind: "new_chat" }), 8);
  now += UNSETTLED_MS;
  assert.equal(await askOnce(c, { kind: "new_chat" }), 7);
});

test("pool.the_tab_that_shows_the_chat_keeps_it_while_its_answer_may_still_be_generating", async () => {
  const fake = poolChrome([poolTab(7, "a"), poolTab(8, "b"), poolTab(9, "c")]);
  const load = new Map<number, number>();
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, (id) => load.get(id) ?? 0);

  // Free, with a recent timeout: it still wins for its chat.
  chooser.noteOutcome(7, "timeout");
  assert.equal(await askOnce(chooser, conversation("a")), 7);
  // Busy too: the ask waits there rather than typing into the same chat
  // from another free tab.
  load.set(7, 1);
  assert.equal(await askOnce(chooser, conversation("a")), 7);
  chooser.noteOutcome(7, "cancelled");
  assert.equal(await askOnce(chooser, conversation("a")), 7);
  assert.equal(fake.createdTabs.length, 0);
  // With its last ask settled, a busy tab with the chat leaves it to a free
  // one.
  chooser.noteOutcome(7, null);
  assert.equal(await askOnce(chooser, conversation("a")), 8);
  // The tab that shows another chat is not held to this one.
  chooser.noteOutcome(9, "timeout");
  assert.equal(await askOnce(chooser, conversation("a")), 8);
});

test("pool.a_cancelled_ask_and_a_dropped_one_leave_no_load_behind", async (t) => {
  quiet(t);
  // The background's flow (bridge.ts): choose, queue, unreserve.
  const fake = poolChrome([poolTab(7, "a")]);
  fake.api.tabs.create = async () => {
    throw new Error("No window with id: 5.");
  };
  const queue = new TabQueue();
  const chooser = new TabChooser(fake.api, fakeMarks().readMark, (id) => queue.pending(id));
  const start = async (deadline: number, job: () => Promise<string>, cancelled = false) => {
    const tabId = await chooser.tabForAsk(conversation("a"));
    if (cancelled) {
      chooser.unreserve(tabId);
      return { tabId, run: Promise.resolve({ ran: false as const }) };
    }
    const run = queue.run(tabId, deadline, job);
    chooser.unreserve(tabId);
    return { tabId, run };
  };

  let finish!: () => void;
  const held = new Promise<void>((r) => (finish = r));
  const running = await start(Date.now() + 60_000, async () => {
    await held;
    return "cancelled by stop";
  });
  assert.equal(queue.pending(7), 1);
  assert.equal(chooser.reservations(7), 0);
  // Waits behind it and is dropped at its deadline, never run.
  const dropped = await start(Date.now() + 20, async () => "never");
  assert.equal(dropped.tabId, 7);
  assert.equal(queue.pending(7), 2);
  // A stop came while its tab was chosen: never queued.
  const stopped = await start(Date.now() + 60_000, async () => "never", true);
  assert.equal(stopped.tabId, 7);
  assert.deepEqual(await dropped.run, { ran: false });
  finish();
  assert.deepEqual(await running.run, { ran: true, value: "cancelled by stop" });

  assert.equal(queue.pending(7), 0);
  assert.equal(chooser.reservations(7), 0);
  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), 7);
});

test("pool.after_a_browser_start_a_duplicated_tab_in_another_window_leaves_everything_ordinary", async (t) => {
  quiet(t);
  const restored: FakeTab[] = [12, 13].map((id) => ({
    id,
    windowId: 11,
    url: `https://chatgpt.com/c/own-${id}`,
    active: id === 12,
  }));
  const local = { ownWindowMark: { token: POOL_TOKEN, windowId: 5, tabIds: [7, 8] } };
  const cases: Array<{ extra: FakeTab[]; marks: Record<number, string>; unreadable: number[] }> = [
    // A copy of 13 moved to a window of its own (Chrome copies sessionStorage).
    {
      extra: [{ id: 20, windowId: 19, url: "https://chatgpt.com/c/own-13", active: true }],
      marks: { 12: POOL_TOKEN, 20: POOL_TOKEN },
      unreadable: [],
    },
    // …or into a window of ChatGPT tabs next to the user's.
    {
      extra: [
        { id: 20, windowId: 19, url: "https://chatgpt.com/c/own-13", active: true },
        { id: 21, windowId: 19, url: "https://chatgpt.com/c/user", active: false },
      ],
      marks: { 12: POOL_TOKEN, 20: POOL_TOKEN },
      unreadable: [],
    },
    // A ChatGPT tab of another such window does not answer: it might.
    {
      extra: [{ id: 20, windowId: 19, url: "https://chatgpt.com/c/user", active: true }],
      marks: { 12: POOL_TOKEN },
      unreadable: [20],
    },
  ];
  for (const c of cases) {
    const fake = fakeChrome({ focusedWindowId: 1, local, tabs: [...restored, ...c.extra] });
    const chooser = new TabChooser(fake.api, fakeMarks(c.marks, c.unreadable).readMark);
    await chooser.noteBrowserStart();
    assert.deepEqual(await chooser.ownTabs(), []);
    assert.equal(fake.session.ownTab, undefined);
    assert.equal(fake.local.ownWindowMark, undefined);
  }

  // The marked tab shares its window with another site: not the own window.
  const mixed = fakeChrome({
    focusedWindowId: 1,
    local,
    tabs: [...restored, { id: 16, windowId: 11, url: "https://example.com/", active: false }],
  });
  const marks = fakeMarks({ 12: POOL_TOKEN });
  const c3 = new TabChooser(mixed.api, marks.readMark);
  await c3.noteBrowserStart();
  assert.deepEqual(await c3.ownTabs(), []);
  assert.deepEqual(marks.asked, []);
});

// ─── adapter ────────────────────────────────────────────────────────────

test("adapter.conversationPathFrom", () => {
  assert.equal(conversationPathFrom("/c/local-chatgpt:4f1c2d3e-0000-4000-8000-000000000000"), null);
  assert.equal(conversationPathFrom("/c/68fd1a2b-1234-8000-9abc-0123456789ab"), "/c/68fd1a2b-1234-8000-9abc-0123456789ab");
  assert.equal(conversationPathFrom("/"), null);
  assert.equal(conversationPathFrom("/c/"), null);
  assert.equal(conversationPathFrom("/library"), null);
});

// ─── pageShim ───────────────────────────────────────────────────────────

test("shim.reports_http_429_as_rate_limited", async (t) => {
  t.mock.method(console, "info", () => {}); // the shim's own debug log
  const posted = installFakePage(async () => new Response("Too many requests", { status: 429 }));
  await import("../src/pageShim");
  const win = (globalThis as unknown as { window: FakeWindow }).window;

  win.postMessage({ __cgptBridgeControl: true, active: true, ttlMs: 60_000 }, "https://chatgpt.com");
  const response = await win.fetch("https://chatgpt.com/backend-api/f/conversation", { method: "POST" });
  win.postMessage({ __cgptBridgeControl: true, active: false }, "https://chatgpt.com");

  // The page still gets the real response…
  assert.equal(response.status, 429);
  // …and the content script gets a stream error carrying the status, which
  // the adapter reads as a rate limit.
  const report = posted.find((d) => d.__cgptBridge === true);
  assert.ok(report, "shim posted nothing to the content script");
  assert.equal(report.kind, "sse-error");
  assert.equal(report.status, 429);
  // …in the epoch the shim acknowledged when switched on (TASK-010).
  const on = posted.find((d) => d.__cgptBridgeAck === true && d.active === true);
  assert.equal(typeof on?.epoch, "number");
  assert.equal(report.epoch, on?.epoch);
  assert.equal(isRateLimitSignal(report), true);
  assert.equal(isRateLimitSignal({ __cgptBridge: true, kind: "sse-error", message: "aborted" }), false);
});

// Just enough of a MAIN-world page for pageShim.ts to install itself.
interface FakeWindow {
  postMessage(data: unknown, origin: string): void;
  fetch(input: string, init?: RequestInit): Promise<Response>;
}

function installFakePage(fetchImpl: () => Promise<Response>): Record<string, unknown>[] {
  const posted: Record<string, unknown>[] = [];
  const listeners: Array<(ev: { source: unknown; data: unknown }) => void> = [];
  const win = {
    location: { origin: "https://chatgpt.com" },
    fetch: fetchImpl,
    addEventListener(type: string, fn: (ev: { source: unknown; data: unknown }) => void) {
      if (type === "message") listeners.push(fn);
    },
    removeEventListener() {},
    postMessage(data: unknown) {
      posted.push(data as Record<string, unknown>);
      for (const fn of [...listeners]) fn({ source: win, data });
    },
  };
  class FakeDocument {
    get hidden(): boolean {
      return false;
    }
    get visibilityState(): string {
      return "visible";
    }
    hasFocus(): boolean {
      return true;
    }
    addEventListener(): void {}
    dispatchEvent(): boolean {
      return true;
    }
  }
  class FakeXhr {
    open(): void {}
    send(): void {}
  }
  const g = globalThis as unknown as Record<string, unknown>;
  g.window = win;
  g.Document = FakeDocument;
  g.document = new FakeDocument();
  g.XMLHttpRequest = FakeXhr;
  return posted;
}
