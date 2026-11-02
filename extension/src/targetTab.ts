// Which ChatGPT tab a bridge request goes to.
//
// Two kinds of requests share the extension:
//   - Gateway requests carry a chat target. They run in the extension's OWN
//     tab: a chatgpt.com tab in a separate window opened without focus, so
//     the user's tabs are never touched. Its ids live in
//     `chrome.storage.session` (survives service-worker restarts, not a
//     browser restart); if the tab is gone, the next targeted ask opens a
//     new window.
//   - That record is gone after an extension reload or update and after a
//     browser restart. The own window is then taken back only by the mark
//     its page carries: a random token the gateway's asks write into the
//     page's sessionStorage (content.ts), equal to the one kept with the
//     window's ids in `chrome.storage.local`. After a reload the recorded
//     tab is asked for it (the ids stay). After a browser start
//     (`noteBrowserStart`; a restored window has new ids) the chatgpt.com
//     tabs alone in a normal window are asked, once, and exactly one of them
//     must carry it while all of them answer. Anything else leaves every
//     tab ordinary, drops the record (no copy of that page is the own
//     window then), and the gateway opens a new window. So does an own tab
//     found closed or off chatgpt.com. Each page gets MARK_READ_TIMEOUT_MS
//     to answer, so a page that hangs holds up no request for long.
//   - Untargeted requests (CLI `cgpt ask` / `cgpt agent`) keep the original
//     rules: the user's active ChatGPT tab, sticky to the tab that answered
//     the previous request. The own tab is never picked for them — it shows
//     some gateway session's chat.
//
// The chrome APIs come in through `TabApi`, so the rules are testable with
// plain fakes (see test/target.test.ts).

import type { ChatTarget } from "./bridgeProtocol";
import { conversationPathOfUrl } from "./chatgptAdapter";

export const OWN_WINDOW_URL = "https://chatgpt.com/";
const CHATGPT_URL_PREFIX = "https://chatgpt.com/";
const OWN_TAB_KEY = "ownTab";
const OWN_MARK_KEY = "ownWindowMark";
// How long a page gets to report its mark; a look for the own window takes
// at most two such rounds (the recorded tab, then the restart scan).
export const MARK_READ_TIMEOUT_MS = 1_500;

/** The subset of `chrome.*` this module uses (promise forms). */
export interface TabApi {
  tabs: {
    get(tabId: number): Promise<chrome.tabs.Tab>;
    query(queryInfo: chrome.tabs.QueryInfo): Promise<chrome.tabs.Tab[]>;
    update(tabId: number, props: chrome.tabs.UpdateProperties): Promise<chrome.tabs.Tab | undefined>;
  };
  windows: {
    create(createData?: chrome.windows.CreateData): Promise<chrome.windows.Window | undefined>;
  };
  storage: {
    session: {
      get(key: string): Promise<Record<string, unknown>>;
      set(items: Record<string, unknown>): Promise<void>;
    };
    local: {
      get(key: string): Promise<Record<string, unknown>>;
      set(items: Record<string, unknown>): Promise<void>;
      remove(key: string): Promise<void>;
    };
  };
}

/**
 * The own-window mark in the page of `tabId`, as its content script reports
 * it: null when the page has none, undefined when the page could not be
 * asked (no content script, still loading, an error page).
 */
export type ReadOwnMark = (tabId: number) => Promise<string | null | undefined>;

interface OwnTabRecord {
  windowId: number;
  tabId: number;
}

// Kept in chrome.storage.local, which outlives a reload and a restart.
interface OwnMarkRecord extends OwnTabRecord {
  token: string;
  // A browser start came after the record was written: the own window, if
  // Chrome restored it, has new ids.
  browserStarted?: true;
}

export class TabChooser {
  // The tab + conversation that answered the previous untargeted request.
  // Follow-up requests (e.g. the next turn of `cgpt agent`) go back there
  // even if the user has since focused another ChatGPT tab. Lives as long as
  // the service worker, which the open native port keeps alive.
  private lastTarget: { tabId: number; path: string } | null = null;
  private opening: Promise<number> | null = null;
  // The look for a marked window (at most one at a time).
  private adopting: Promise<OwnTabRecord | null> | null = null;
  // `onStartup` came and the restart scan has not run yet.
  private browserStarted = false;
  // The recorded tab while it did not answer: possibly the own one, so the
  // CLI does not take it either.
  private inDoubt: number | null = null;
  // Read-modify-writes of the mark record, one at a time.
  private markWrites: Promise<unknown> = Promise.resolve();

  constructor(
    private readonly api: TabApi,
    private readonly readMark: ReadOwnMark = async () => undefined,
  ) {}

  /** Tab for an `ask`: the own tab (opened if needed) when targeted. */
  async tabForAsk(target: ChatTarget | undefined): Promise<number> {
    if (target === undefined) return this.legacyTab();
    const own = await this.ownTab();
    if (own !== null && typeof own.id === "number") return own.id;
    return this.openOwnWindow();
  }

  /** The own tab if it is still open on chatgpt.com. Never opens one. */
  async ownTab(): Promise<chrome.tabs.Tab | null> {
    const record = await this.ownRecord();
    if (record === null) return null;
    const tab = await this.api.tabs.get(record.tabId).catch(() => null); // tab or window closed
    // While the first load is in flight only `pendingUrl` is set.
    const url = tab?.pendingUrl || tab?.url;
    if (tab !== null && typeof url === "string" && url.startsWith(CHATGPT_URL_PREFIX)) return tab;
    // That window is gone (or left ChatGPT): a copy of its page, say a
    // reopened tab, must not be taken back later.
    await this.dropMark((m) => m.tabId === record.tabId);
    return null;
  }

  /** Remember the tab that answered an untargeted ask (sticky follow-ups). */
  async rememberLegacyTarget(tabId: number): Promise<void> {
    try {
      const tab = await this.api.tabs.get(tabId);
      const path = conversationPathOfUrl(tab.url);
      this.lastTarget = path ? { tabId, path } : null;
    } catch {
      this.lastTarget = null;
    }
  }

  /** The mark a targeted ask writes into the page of `tabId`, the own tab only. */
  async ownMarkFor(tabId: number): Promise<string | null> {
    const mark = await this.markRecord();
    return mark !== null && mark.tabId === tabId ? mark.token : null;
  }

  /** The browser started (`chrome.runtime.onStartup`): see `adopt`. */
  async noteBrowserStart(): Promise<void> {
    this.browserStarted = true;
    // For a service worker that starts later in this browser session.
    await this.withMarkRecord(async () => {
      const mark = await this.markRecord();
      if (mark !== null) {
        await this.api.storage.local.set({ [OWN_MARK_KEY]: { ...mark, browserStarted: true } });
      }
    });
  }

  // `write` after the writes queued before it. Every change of the mark
  // record goes through here, so a new window's record is never overwritten
  // by an older one read just before.
  private withMarkRecord<T>(write: () => Promise<T>): Promise<T> {
    const next = this.markWrites.then(write);
    this.markWrites = next.catch(() => undefined);
    return next;
  }

  // Removes the mark record if `which` says it is the one to go.
  private async dropMark(which: (mark: OwnMarkRecord) => boolean): Promise<void> {
    await this.withMarkRecord(async () => {
      const mark = await this.markRecord();
      if (mark === null || !which(mark)) return;
      await this.api.storage.local.remove(OWN_MARK_KEY);
    }).catch((err: unknown) => {
      console.warn("[cgpt-bridge] could not drop the gateway window's mark record:", err);
    });
    this.inDoubt = null;
  }

  private async ownRecord(): Promise<OwnTabRecord | null> {
    const items = await this.api.storage.session.get(OWN_TAB_KEY);
    const v = items[OWN_TAB_KEY] as Partial<OwnTabRecord> | undefined;
    if (!v || typeof v.tabId !== "number" || typeof v.windowId !== "number") {
      return this.adoptOnce();
    }
    return { windowId: v.windowId, tabId: v.tabId };
  }

  // Null as well when the storage cannot be read: an ask then goes without
  // the mark, and nothing is taken back.
  private async markRecord(): Promise<OwnMarkRecord | null> {
    let items: Record<string, unknown>;
    try {
      items = await this.api.storage.local.get(OWN_MARK_KEY);
    } catch (err) {
      console.warn("[cgpt-bridge] could not read the gateway window's mark record:", err);
      return null;
    }
    const v = items[OWN_MARK_KEY] as Partial<OwnMarkRecord> | undefined;
    if (
      !v ||
      typeof v.token !== "string" ||
      v.token === "" ||
      typeof v.tabId !== "number" ||
      typeof v.windowId !== "number"
    ) {
      return null;
    }
    return {
      token: v.token,
      windowId: v.windowId,
      tabId: v.tabId,
      ...(v.browserStarted === true ? { browserStarted: true } : {}),
    };
  }

  private adoptOnce(): Promise<OwnTabRecord | null> {
    if (this.adopting === null) {
      this.adopting = this.adopt()
        .catch((err: unknown) => {
          console.warn("[cgpt-bridge] could not look for the gateway window:", err);
          return null;
        })
        .finally(() => {
          this.adopting = null;
        });
    }
    return this.adopting;
  }

  // The own window after the session record was lost (see the header), or
  // null. Every answer but "a page did not answer" on the reload path drops
  // the record, so the next look costs one storage read.
  private async adopt(): Promise<OwnTabRecord | null> {
    // No record (no gateway window yet): one storage read, nothing asked.
    const mark = await this.markRecord();
    if (mark === null) return null;
    const restart = this.browserStarted || mark.browserStarted === true;
    const same = (m: OwnMarkRecord): boolean => m.token === mark.token;

    // An extension reload or update keeps the ids: the recorded tab, if its
    // page still carries the mark. Tab ids are not reused within a browser
    // session, and after a restart a tab that got the same id has no mark.
    const recorded = await this.api.tabs.get(mark.tabId).catch(() => null);
    if (recorded !== null && recorded.windowId === mark.windowId && isChatGptTab(recorded)) {
      const found = await this.readTabMark(recorded);
      if (found === mark.token) return this.takeBack(mark, recorded.id as number);
      if (found === undefined && !restart) {
        // Still loading, say: ask again next time, and keep the CLI off it.
        this.inDoubt = mark.tabId;
        return null;
      }
    }
    if (!restart) {
      await this.dropMark(same);
      return null;
    }

    // After a browser start Chrome restores the window under new ids. Only
    // a chatgpt.com tab alone in a normal window is asked (the own window
    // has one tab, and the user's other tabs are left alone), and it must be
    // the only one with the mark: a duplicated tab carries it too. This is
    // done once: a copy of the page made later must not pass as the window.
    this.browserStarted = false;
    const normal = await this.api.tabs.query({ windowType: "normal" });
    const perWindow = new Map<number, number>();
    for (const t of normal) perWindow.set(t.windowId, (perWindow.get(t.windowId) ?? 0) + 1);
    const lone = normal.filter(
      (t): t is chrome.tabs.Tab & { id: number } =>
        isChatGptTab(t) && perWindow.get(t.windowId) === 1,
    );
    const found = await Promise.all(lone.map((t) => this.readTabMark(t)));
    const marked = lone.filter((_t, i) => found[i] === mark.token);
    const tab = marked[0];
    if (marked.length === 1 && tab !== undefined && !found.includes(undefined)) {
      return this.takeBack(mark, tab.id);
    }
    if (marked.length > 1) {
      console.warn(
        `[cgpt-bridge] ${marked.length} tabs carry the gateway window's mark; none is taken as the gateway window`,
      );
    } else if (marked.length === 1) {
      console.warn(
        "[cgpt-bridge] the gateway window is not taken back: a ChatGPT tab did not say whether it carries its mark",
      );
    }
    await this.dropMark(same);
    return null;
  }

  // The mark in `tab`'s page; undefined when it cannot answer (discarded,
  // or no answer within MARK_READ_TIMEOUT_MS).
  private async readTabMark(tab: chrome.tabs.Tab): Promise<string | null | undefined> {
    if (tab.discarded === true || typeof tab.id !== "number") return undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeout = new Promise<undefined>((resolve) => {
      timer = setTimeout(() => resolve(undefined), MARK_READ_TIMEOUT_MS);
    });
    try {
      return await Promise.race([this.readMark(tab.id).catch(() => undefined), timeout]);
    } finally {
      clearTimeout(timer);
    }
  }

  private async takeBack(mark: OwnMarkRecord, tabId: number): Promise<OwnTabRecord> {
    const tab = await this.api.tabs.get(tabId);
    const record: OwnTabRecord = { windowId: tab.windowId, tabId };
    await this.withMarkRecord(() =>
      this.api.storage.local.set({ [OWN_MARK_KEY]: { token: mark.token, ...record } }),
    );
    await this.api.storage.session.set({ [OWN_TAB_KEY]: record });
    this.inDoubt = null;
    await this.keepLoaded(tabId);
    console.info(`[cgpt-bridge] took back the gateway window (tab ${tabId}) by its page's mark`);
    return record;
  }

  // A long-lived tab in a background window is a Memory Saver candidate;
  // an unloaded tab has no content script to answer.
  private async keepLoaded(tabId: number): Promise<void> {
    try {
      await this.api.tabs.update(tabId, { autoDiscardable: false });
    } catch (err) {
      console.warn("[cgpt-bridge] could not pin the gateway tab against discarding:", err);
    }
  }

  private openOwnWindow(): Promise<number> {
    // Two asks racing here must not open two windows.
    if (this.opening === null) {
      this.opening = this.createOwnWindow().finally(() => {
        this.opening = null;
      });
    }
    return this.opening;
  }

  private async createOwnWindow(): Promise<number> {
    // Another path may have opened it since the caller looked.
    const existing = await this.ownTab();
    if (existing !== null && typeof existing.id === "number") return existing.id;
    const win = await this.api.windows.create({
      url: OWN_WINDOW_URL,
      focused: false,
      state: "normal",
    });
    const tabId = win?.tabs?.[0]?.id;
    if (typeof win?.id !== "number" || typeof tabId !== "number") {
      throw new Error("could not open a ChatGPT window for the gateway");
    }
    const record: OwnTabRecord = { windowId: win.id, tabId };
    // The mark first: a session record without it could not be taken back.
    const mark: OwnMarkRecord = { token: crypto.randomUUID(), ...record };
    await this.withMarkRecord(() => this.api.storage.local.set({ [OWN_MARK_KEY]: mark }));
    this.inDoubt = null;
    await this.api.storage.session.set({ [OWN_TAB_KEY]: record });
    await this.keepLoaded(tabId);
    return tabId;
  }

  private async legacyTab(): Promise<number> {
    // Preference order:
    //   1. The tab that answered the previous request, if it still shows the
    //      same conversation — keeps a multi-turn session in one chat.
    //   2. The active chatgpt.com tab in the last-focused window (matches the
    //      user's mental model: "the chat I am looking at").
    //   3. The active chatgpt.com tab in any other window.
    //   4. The only chatgpt.com tab, when there is exactly one. With several
    //      inactive ones we refuse rather than guess and post into the wrong
    //      conversation.
    // The own tab is excluded from every step, and so is the recorded one
    // while it did not say whether it is the own one.
    const ownId = (await this.ownTab())?.id ?? this.inDoubt;
    const usable = (t: chrome.tabs.Tab): t is chrome.tabs.Tab & { id: number } =>
      isChatGptTab(t) && t.id !== ownId;

    if (this.lastTarget !== null) {
      try {
        const tab = await this.api.tabs.get(this.lastTarget.tabId);
        if (usable(tab) && conversationPathOfUrl(tab.url) === this.lastTarget.path) {
          return tab.id;
        }
      } catch {
        // tab closed
      }
      this.lastTarget = null;
    }

    const activeQueries: chrome.tabs.QueryInfo[] = [
      { active: true, lastFocusedWindow: true, url: `${CHATGPT_URL_PREFIX}*` },
      { active: true, url: `${CHATGPT_URL_PREFIX}*` },
    ];
    for (const q of activeQueries) {
      const tab = (await this.api.tabs.query(q)).find(usable);
      if (tab) return tab.id;
    }

    const all = (await this.api.tabs.query({ url: `${CHATGPT_URL_PREFIX}*` })).filter(usable);
    if (all.length === 1 && all[0]) return all[0].id;
    if (all.length > 1) {
      throw new Error(
        `${all.length} chatgpt.com tabs are open and none is active — ` +
          "switch to the ChatGPT tab you want to use and retry",
      );
    }
    throw new Error(
      "no https://chatgpt.com/ tab is open — open chatgpt.com in any Chrome window",
    );
  }
}

function isChatGptTab(t: chrome.tabs.Tab): boolean {
  return (
    typeof t.id === "number" &&
    typeof t.url === "string" &&
    t.url.startsWith(CHATGPT_URL_PREFIX)
  );
}
