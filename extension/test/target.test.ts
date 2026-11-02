// Pure-logic tests for the gateway side of the extension: which tab a request
// goes to, how chat paths are read, and how the page shim reports HTTP 429.
// Run with `npm run test` (esbuild bundle + node:test, no browser).

import { test } from "node:test";
import assert from "node:assert/strict";

import { TabChooser, type TabApi } from "../src/targetTab";
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
  return { api, tabs, session, local, createdWindows, updates };
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

  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), 7);
  assert.equal(
    await chooser.tabForAsk({ kind: "conversation", url: "https://chatgpt.com/c/other" }),
    7,
  );
  assert.equal(fake.createdWindows.length, 0);
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
  assert.deepEqual(fake.session.ownTab, { windowId: 100, tabId });
  // Memory Saver must not unload the long-lived gateway tab.
  assert.deepEqual(fake.updates, [[tabId, { autoDiscardable: false }]]);

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

  const rec = fake.local.ownWindowMark as { token: string; windowId: number; tabId: number };
  assert.equal(rec.tabId, tabId);
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
  assert.equal(await chooser.tabForAsk({ kind: "new_chat" }), 7);
  assert.equal(fake.createdWindows.length, 0);
  assert.deepEqual(fake.session.ownTab, OWN);
  assert.deepEqual(fake.updates, [[7, { autoDiscardable: false }]]);
  assert.equal((await chooser.ownTab())?.id, 7);
  // Taken back once: no more questions to the pages.
  const asked = marks.asked.length;
  await chooser.tabForAsk(undefined);
  await chooser.tabForAsk({ kind: "new_chat" });
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

test("targetTab.after_a_restart_only_lone_chatgpt_tabs_are_asked_and_all_must_answer", async (t) => {
  quiet(t);
  // Tabs that share a window with others, and tabs on other sites, are
  // never asked.
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
  assert.deepEqual([...marks.asked].sort((x, y) => x - y), [12, 15]);

  // A lone tab that cannot answer (discarded, or not loaded yet) might
  // carry the mark too: nothing is taken. The scan runs once, so the record
  // goes, and the window stays ordinary once the tab can answer.
  for (const silent of [{ ...tabs[4]!, discarded: true }, tabs[4]!]) {
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

test("targetTab.an_own_tab_that_left_chatgpt_drops_the_mark_record", async (t) => {
  quiet(t);
  const fake = fakeChrome({
    focusedWindowId: 1,
    session: { ownTab: OWN },
    local: { ownWindowMark: MARK_RECORD },
    tabs: [{ ...ownTab, url: "https://example.com/" }],
  });
  assert.equal(await new TabChooser(fake.api).ownTab(), null);
  assert.equal(fake.local.ownWindowMark, undefined);
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
  const rec = fake.local.ownWindowMark as { token: string; tabId: number; browserStarted?: boolean };
  assert.equal(rec.tabId, tabId);
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
  assert.deepEqual(started.session.ownTab, { windowId: 11, tabId: 12 });
  assert.deepEqual(started.local.ownWindowMark, { token: TOKEN, windowId: 11, tabId: 12 });
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
