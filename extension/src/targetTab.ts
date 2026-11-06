// Which ChatGPT tab a bridge request goes to.
//
// Two kinds of requests share the extension:
//   - Gateway requests carry a chat target. They run in the extension's OWN
//     window: a separate window opened without focus, holding a pool of
//     chatgpt.com tabs (at most OWN_TABS_MAX), so several gateway sessions
//     are answered at once and the user's tabs are never touched. A targeted
//     ask goes to the free pool tab that already shows its chat, else to any
//     free pool tab, else to a new tab in the own window while the pool is
//     not full, else to the least loaded pool tab, where it waits its turn
//     (a tab is free when no ask runs or waits in it). A tab whose last ask
//     ended in a timeout, a stop or a busy page may still be generating:
//     while it shows the target chat it keeps that chat, busy or not;
//     otherwise it comes after the other free tabs. The window's id and the pool's
//     tab ids live in `chrome.storage.session` (survives service-worker
//     restarts, not a browser restart); a pool tab found closed or moved out
//     of the window leaves the pool, one away from chatgpt.com (a sign-in
//     page, say) stays in it but gets no request until it is back, and with
//     no pool tab left the next targeted ask opens a new window.
//   - That record is gone after an extension reload or update and after a
//     browser restart. The own window is then taken back only by the mark
//     its pages carry: a random token the gateway's asks write into the
//     page's sessionStorage (content.ts), equal to the one kept with the
//     window's ids in `chrome.storage.local`. After a reload the recorded
//     tabs are asked for it (the ids stay); once one of them carries it the
//     window is proven, and the recorded tabs still in it and on chatgpt.com
//     go back into the pool. After a browser start (`noteBrowserStart`; a
//     restored window has new ids) the normal windows whose tabs are all on
//     chatgpt.com are asked, once: exactly one of them must have a tab with
//     the mark, and every loaded tab of the others must answer; the tabs of
//     that window that carry the mark then form the pool (its restored tabs
//     not loaded yet are loaded in the background and join once they report
//     the mark). Anything else
//     leaves every tab ordinary, drops the
//     record (no copy of a page is the own window then), and the gateway
//     opens a new window. Each page gets MARK_READ_TIMEOUT_MS to answer, so
//     a page that hangs holds up no request for long.
//   - Untargeted requests (CLI `cgpt ask` / `cgpt agent`) keep the original
//     rules: the user's active ChatGPT tab, sticky to the tab that answered
//     the previous request. No tab of the own window is ever picked for
//     them — the pool tabs show gateway sessions' chats.
//
// The chrome APIs come in through `TabApi`, and the load of a tab (asks
// running or queued in it) through `TabLoad`, so the rules are testable with
// plain fakes (see test/target.test.ts).

import type { ChatTarget } from "./bridgeProtocol";
import { conversationPathOfUrl } from "./chatgptAdapter";

export const OWN_WINDOW_URL = "https://chatgpt.com/";
// How many tabs the own window holds at most; `status` reports it to the
// gateway (`own_tabs`), which runs no more turns at once than this.
export const OWN_TABS_MAX = 8;
const CHATGPT_URL_PREFIX = "https://chatgpt.com/";
// How long a tab whose ask ended in one of UNSETTLED_CODES may still be
// generating that answer, so other free tabs go first.
export const UNSETTLED_MS = 120_000;
const UNSETTLED_CODES = new Set(["timeout", "cancelled", "tab_busy"]);
// How long a restored tab of the own window gets to load after a takeover.
const RESTORE_LOAD_TIMEOUT_MS = 60_000;
const RESTORE_POLL_MS = 500;
const OWN_TAB_KEY = "ownTab";
const OWN_MARK_KEY = "ownWindowMark";
// How long a page gets to report its mark; a look for the own window takes
// at most two such rounds (the recorded tabs, then the restart scan).
export const MARK_READ_TIMEOUT_MS = 1_500;

/** The subset of `chrome.*` this module uses (promise forms). */
export interface TabApi {
  tabs: {
    get(tabId: number): Promise<chrome.tabs.Tab>;
    query(queryInfo: chrome.tabs.QueryInfo): Promise<chrome.tabs.Tab[]>;
    update(tabId: number, props: chrome.tabs.UpdateProperties): Promise<chrome.tabs.Tab | undefined>;
    create(props: chrome.tabs.CreateProperties): Promise<chrome.tabs.Tab>;
    // Closes a tab opened for the pool that could not be recorded.
    remove?(tabId: number): Promise<void>;
    // Loads a restored tab Chrome has not loaded yet (in the background).
    reload?(tabId: number): Promise<void>;
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

/** How many asks run or wait in `tabId` (the background's TabQueue). */
export type TabLoad = (tabId: number) => number;

type LiveTab = chrome.tabs.Tab & { id: number };

interface OwnPoolRecord {
  windowId: number;
  tabIds: number[];
}

// Kept in chrome.storage.local, which outlives a reload and a restart.
interface OwnMarkRecord extends OwnPoolRecord {
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
  // Targeted choices, one at a time: two asks never get the same free tab,
  // and never open two windows or more tabs than OWN_TABS_MAX.
  private choosing: Promise<unknown> = Promise.resolve();
  // Pool tabs handed out by `tabForAsk` whose ask is not in the tab's queue
  // yet (see `unreserve`); they count as load.
  private reserved = new Map<number, number>();
  // The look for a marked window (at most one at a time).
  private adopting: Promise<OwnPoolRecord | null> | null = null;
  // `onStartup` came and the restart scan has not run yet.
  private browserStarted = false;
  // The recorded tabs while none of them said whether it is an own one:
  // possibly own, so the CLI does not take them either.
  private inDoubt = new Set<number>();
  // Read-modify-writes of the records, one at a time.
  private recordWrites: Promise<unknown> = Promise.resolve();
  // Pool tabs whose last ask ended in one of UNSETTLED_CODES, and when.
  private unsettled = new Map<number, number>();
  // See `restoreUnloaded`.
  private restoring: Promise<void> = Promise.resolve();

  constructor(
    private readonly api: TabApi,
    private readonly readMark: ReadOwnMark = async () => undefined,
    private readonly load: TabLoad = () => 0,
    private readonly now: () => number = Date.now,
  ) {}

  /**
   * Tab for an `ask`: a pool tab of the own window (opened if needed) when
   * targeted. The pool tab comes reserved: the caller puts the ask into the
   * tab's queue and then calls `unreserve` (also when the ask goes no
   * further), so the next choice already sees the tab as busy.
   */
  async tabForAsk(target: ChatTarget | undefined): Promise<number> {
    if (target === undefined) return this.legacyTab();
    const next = this.choosing.then(() => this.choosePoolTab(target));
    this.choosing = next.catch(() => undefined);
    return next;
  }

  /** The ask given `tabId` by `tabForAsk` is in the tab's queue (or ended). */
  unreserve(tabId: number): void {
    const left = (this.reserved.get(tabId) ?? 0) - 1;
    if (left > 0) this.reserved.set(tabId, left);
    else this.reserved.delete(tabId);
  }

  /** Asks handed out for `tabId` and not in its queue yet. */
  reservations(tabId: number): number {
    return this.reserved.get(tabId) ?? 0;
  }

  /**
   * A targeted ask in `tabId` ended, with the wire error code or null. After
   * a timeout, a stop or a busy page the answer may still be generating
   * there, so for UNSETTLED_MS other free tabs are chosen first.
   */
  noteOutcome(tabId: number, code: string | null): void {
    if (code !== null && UNSETTLED_CODES.has(code)) this.unsettled.set(tabId, this.now());
    else this.unsettled.delete(tabId);
  }

  /** The pool tabs in the own window that are on chatgpt.com. Never opens one. */
  async ownTabs(): Promise<LiveTab[]> {
    return (await this.pool())?.members.filter(onChatGptTab) ?? [];
  }

  // The own window and its pool tabs (also those away from chatgpt.com for
  // now), or null. A tab closed or moved out of the window leaves the pool
  // for good: a copy of its page, say a reopened tab, must not be taken
  // back later.
  private async pool(): Promise<{ windowId: number; members: LiveTab[] } | null> {
    const record = await this.poolRecord();
    if (record === null) return null;
    const found = await Promise.all(
      record.tabIds.map((id) => this.api.tabs.get(id).catch(() => null)), // tab or window closed
    );
    const members: LiveTab[] = [];
    const gone: number[] = [];
    found.forEach((tab, i) => {
      if (tab !== null && tab.windowId === record.windowId) members.push(tab as LiveTab);
      else gone.push(record.tabIds[i] as number);
    });
    if (gone.length > 0) await this.dropFromPool(record.windowId, gone);
    return members.length === 0 ? null : { windowId: record.windowId, members };
  }

  /** The first pool tab, or null. Never opens one. */
  async ownTab(): Promise<LiveTab | null> {
    return (await this.ownTabs())[0] ?? null;
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

  /** The mark a targeted ask writes into the page of `tabId`: pool tabs only. */
  async ownMarkFor(tabId: number): Promise<string | null> {
    const mark = await this.markRecord();
    return mark !== null && mark.tabIds.includes(tabId) ? mark.token : null;
  }

  /** The browser started (`chrome.runtime.onStartup`): see `adopt`. */
  async noteBrowserStart(): Promise<void> {
    this.browserStarted = true;
    // For a service worker that starts later in this browser session.
    await this.withRecords(async () => {
      const mark = await this.markRecord();
      if (mark !== null) {
        await this.api.storage.local.set({ [OWN_MARK_KEY]: { ...mark, browserStarted: true } });
      }
    });
  }

  private async choosePoolTab(target: ChatTarget): Promise<number> {
    const pool = await this.pool();
    if (pool === null) return this.reserve(await this.openOwnWindow());
    // A tab away from chatgpt.com (a sign-in page) gets no ask until it is back.
    const onSite = pool.members.filter(onChatGptTab);

    const load = (t: LiveTab): number => this.load(t.id) + this.reservations(t.id);
    const want = target.kind === "conversation" ? conversationPathOfUrl(target.url) : null;
    const shows = (t: LiveTab): boolean =>
      want !== null && conversationPathOfUrl(t.pendingUrl || t.url) === want;
    // A discarded tab has no page to answer until it loads again: the last
    // choice.
    const free = onSite.filter((t) => t.discarded !== true && load(t) === 0);
    // The free tab that shows the chat wins, also while its last answer may
    // still be generating: the page there sees that and waits or refuses.
    const freeInChat = free.find(shows);
    if (freeInChat !== undefined) return this.reserve(freeInChat.id);
    // A tab that shows the chat and may still be generating in it keeps the
    // chat even when busy: another tab typing into it meanwhile would make
    // ChatGPT refuse the turn or branch the chat.
    const generatingChat = onSite.filter((t) => shows(t) && this.isUnsettled(t.id));
    if (generatingChat.length > 0) {
      const byLoad = [...generatingChat].sort((a, b) => load(a) - load(b));
      return this.reserve((byLoad[0] as LiveTab).id);
    }
    // Otherwise a tab whose last ask may still be generating comes after
    // the other free ones.
    const freeTab = free.find((t) => !this.isUnsettled(t.id)) ?? free[0];
    if (freeTab !== undefined) return this.reserve(freeTab.id);

    if (pool.members.length < OWN_TABS_MAX) {
      try {
        return this.reserve(await this.openPoolTab(pool.windowId));
      } catch (err) {
        console.warn("[cgpt-bridge] could not open another tab in the gateway window:", err);
      }
    }

    // Least loaded; on a tie the one that shows the chat, then pool order.
    const rank = (t: LiveTab): number[] => [t.discarded === true ? 1 : 0, load(t), shows(t) ? 0 : 1];
    const byRank = [...onSite].sort((a, b) => compareRanks(rank(a), rank(b)));
    const least = byRank[0];
    if (least === undefined) {
      throw new Error(
        "no tab of the gateway window is on chatgpt.com (a sign-in page?) — sign in to ChatGPT there",
      );
    }
    return this.reserve(least.id);
  }

  private isUnsettled(tabId: number): boolean {
    const at = this.unsettled.get(tabId);
    if (at === undefined) return false;
    if (this.now() - at < UNSETTLED_MS) return true;
    this.unsettled.delete(tabId);
    return false;
  }

  private reserve(tabId: number): number {
    this.reserved.set(tabId, (this.reserved.get(tabId) ?? 0) + 1);
    return tabId;
  }

  // `write` after the writes queued before it. Every change of the records
  // goes through here, so a new window's record is never overwritten by an
  // older one read just before, and a tab added to the pool is never lost to
  // a concurrent removal of another.
  private withRecords<T>(write: () => Promise<T>): Promise<T> {
    const next = this.recordWrites.then(write);
    this.recordWrites = next.catch(() => undefined);
    return next;
  }

  // Removes `tabIds` of window `windowId` from both records; with no tab
  // left the mark record goes (nothing to take back any more).
  private async dropFromPool(windowId: number, tabIds: number[]): Promise<void> {
    const drop = (ids: number[]): number[] => ids.filter((id) => !tabIds.includes(id));
    await this.withRecords(async () => {
      const pool = await this.sessionRecord();
      if (pool !== null && pool.windowId === windowId) {
        await this.api.storage.session.set({
          [OWN_TAB_KEY]: { windowId, tabIds: drop(pool.tabIds) },
        });
      }
      const mark = await this.markRecord();
      if (mark === null || mark.windowId !== windowId) return;
      const left = drop(mark.tabIds);
      if (left.length === 0) await this.api.storage.local.remove(OWN_MARK_KEY);
      else await this.api.storage.local.set({ [OWN_MARK_KEY]: { ...mark, tabIds: left } });
    }).catch((err: unknown) => {
      console.warn("[cgpt-bridge] could not drop closed tabs from the gateway window's records:", err);
    });
    for (const id of tabIds) this.inDoubt.delete(id);
  }

  // Removes the mark record if it is the one with `token`.
  private async dropMark(token: string): Promise<void> {
    await this.withRecords(async () => {
      const mark = await this.markRecord();
      if (mark === null || mark.token !== token) return;
      await this.api.storage.local.remove(OWN_MARK_KEY);
    }).catch((err: unknown) => {
      console.warn("[cgpt-bridge] could not drop the gateway window's mark record:", err);
    });
    this.inDoubt.clear();
  }

  // Both records for a window that is ours now, with a new or the kept token.
  private async writeRecords(token: string, pool: OwnPoolRecord): Promise<void> {
    await this.withRecords(async () => {
      // The mark first: a session record without it could not be taken back.
      await this.api.storage.local.set({ [OWN_MARK_KEY]: { token, ...pool } });
      await this.api.storage.session.set({ [OWN_TAB_KEY]: pool });
    });
    this.inDoubt.clear();
  }

  // The session record as stored, nothing adopted. The single-tab shape of
  // an earlier version (`tabId`) reads as a pool of one.
  private async sessionRecord(): Promise<OwnPoolRecord | null> {
    const items = await this.api.storage.session.get(OWN_TAB_KEY);
    return parsePool(items[OWN_TAB_KEY]);
  }

  private async poolRecord(): Promise<OwnPoolRecord | null> {
    const record = await this.sessionRecord();
    if (record === null || record.tabIds.length === 0) return this.adoptOnce();
    return record;
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
    const pool = parsePool(v);
    if (pool === null || pool.tabIds.length === 0 || typeof v?.token !== "string" || v.token === "") {
      return null;
    }
    return {
      token: v.token,
      ...pool,
      ...(v.browserStarted === true ? { browserStarted: true } : {}),
    };
  }

  private adoptOnce(): Promise<OwnPoolRecord | null> {
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
  // null. Every answer but "no page answered" on the reload path drops the
  // record, so the next look costs one storage read.
  private async adopt(): Promise<OwnPoolRecord | null> {
    // No record (no gateway window yet): one storage read, nothing asked.
    const mark = await this.markRecord();
    if (mark === null) return null;
    const restart = this.browserStarted || mark.browserStarted === true;

    // An extension reload or update keeps the ids: the recorded tabs, if one
    // of them still carries the mark. Tab ids are not reused within a
    // browser session, and after a restart a tab that got a recorded id has
    // no mark. Once one page proves the window, the other recorded tabs in
    // it are ours by their ids — also one without the mark, which only a
    // tab whose first ask never reached its page lacks, and one that does
    // not answer now (still loading); never one with another window's mark.
    const recorded = (
      await Promise.all(mark.tabIds.map((id) => this.api.tabs.get(id).catch(() => null)))
    ).filter(
      (t): t is LiveTab => t !== null && t.windowId === mark.windowId && isChatGptTab(t),
    );
    const found = await Promise.all(recorded.map((t) => this.readTabMark(t)));
    if (found.includes(mark.token)) {
      const pool = recorded.filter((_t, i) => found[i] === mark.token || !isMark(found[i]));
      return this.takeBack(mark, mark.windowId, pool);
    }
    if (!restart) {
      if (found.includes(undefined)) {
        // Still loading, say: ask again next time, and keep the CLI off
        // every tab that joins the pool if the silent one proves the window.
        this.inDoubt = new Set(recorded.filter((_t, i) => !isMark(found[i])).map((t) => t.id));
        return null;
      }
      await this.dropMark(mark.token);
      return null;
    }

    // After a browser start Chrome restores the window under new ids. Only
    // the tabs of normal windows whose tabs are all on chatgpt.com are asked
    // (the own window holds nothing else, and the user's other windows are
    // left alone). Exactly one such window may have a tab with the mark: a
    // duplicated tab carries it too. Every loaded tab of the other windows
    // must answer, as a silent one might carry it; a tab Chrome has not
    // loaded yet (restored tabs load lazily) is not waited for. Only the
    // tabs that carry the mark join the pool: an unmarked one may be a tab
    // the user opened in that window, and a silent one cannot be told. Both
    // stay out (the CLI does not take them either, they are in the own
    // window). This is done once: a copy of a page made later must not pass
    // as the window.
    this.browserStarted = false;
    const normal = await this.api.tabs.query({ windowType: "normal" });
    const perWindow = new Map<number, chrome.tabs.Tab[]>();
    for (const t of normal) perWindow.set(t.windowId, [...(perWindow.get(t.windowId) ?? []), t]);
    const asked = [...perWindow.values()]
      .filter((tabs) => tabs.every(isChatGptTab))
      .flat() as LiveTab[];
    const answers = await Promise.all(asked.map((t) => this.readTabMark(t)));
    const markedWindows = new Set(
      asked.filter((_t, i) => answers[i] === mark.token).map((t) => t.windowId),
    );
    const [windowId] = [...markedWindows];
    const othersAnswered = asked.every(
      (t, i) => t.windowId === windowId || answers[i] !== undefined || notLoaded(t),
    );
    if (markedWindows.size === 1 && windowId !== undefined && othersAnswered) {
      const pool = asked.filter((t, i) => t.windowId === windowId && answers[i] === mark.token);
      const record = await this.takeBack(mark, windowId, pool);
      // Restored tabs of the window Chrome has not loaded yet: loaded in the
      // background, and asked once they are; they join only with the mark.
      // The ask that brought the takeover does not wait for this.
      const unloaded = asked.filter((t) => t.windowId === windowId && notLoaded(t));
      if (unloaded.length > 0) this.restoring = this.restoreUnloaded(mark.token, windowId, unloaded);
      return record;
    }
    if (markedWindows.size > 1) {
      console.warn(
        `[cgpt-bridge] ${markedWindows.size} windows have tabs with the gateway window's mark; none is taken as the gateway window`,
      );
    } else if (markedWindows.size === 1) {
      console.warn(
        "[cgpt-bridge] the gateway window is not taken back: a ChatGPT tab did not say whether it carries its mark",
      );
    }
    await this.dropMark(mark.token);
    return null;
  }

  // The mark in `tab`'s page; undefined when it cannot answer (discarded,
  // or no answer within MARK_READ_TIMEOUT_MS).
  private async readTabMark(tab: chrome.tabs.Tab): Promise<string | null | undefined> {
    if (notLoaded(tab) || typeof tab.id !== "number") return undefined;
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

  // Loads each tab once (no focus change) and adds those that then carry
  // `token` to the pool, if the pool is still that window's. Best effort.
  private async restoreUnloaded(token: string, windowId: number, tabs: LiveTab[]): Promise<void> {
    await Promise.all(
      tabs.map(async (tab) => {
        try {
          if (this.api.tabs.reload === undefined) return;
          await this.api.tabs.reload(tab.id);
          const loaded = await this.loadedTab(tab.id);
          if (loaded === null || loaded.windowId !== windowId) return;
          if ((await this.readTabMark(loaded)) !== token) return;
          await this.addToPool(windowId, tab.id, token);
          await this.keepLoaded(tab.id);
        } catch (err) {
          console.warn(`[cgpt-bridge] could not bring back restored gateway tab ${tab.id}:`, err);
        }
      }),
    );
  }

  // The tab once it has loaded, or null when it is gone or does not load
  // within RESTORE_LOAD_TIMEOUT_MS.
  private async loadedTab(tabId: number): Promise<chrome.tabs.Tab | null> {
    for (let waited = 0; ; waited += RESTORE_POLL_MS) {
      const tab = await this.api.tabs.get(tabId).catch(() => null);
      if (tab === null) return null;
      if (tab.status === "complete" && !notLoaded(tab)) return tab;
      if (waited >= RESTORE_LOAD_TIMEOUT_MS) return null;
      await new Promise((r) => setTimeout(r, RESTORE_POLL_MS));
    }
  }

  /** The background loads of restored tabs the last takeover started. */
  whenRestored(): Promise<void> {
    return this.restoring;
  }

  private async takeBack(
    mark: OwnMarkRecord,
    windowId: number,
    tabs: LiveTab[],
  ): Promise<OwnPoolRecord> {
    const record: OwnPoolRecord = { windowId, tabIds: tabs.map((t) => t.id) };
    await this.writeRecords(mark.token, record);
    await Promise.all(record.tabIds.map((id) => this.keepLoaded(id)));
    console.info(
      `[cgpt-bridge] took back the gateway window (tabs ${record.tabIds.join(", ")}) by its pages' mark`,
    );
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

  // Called only from a choice, so never twice at once.
  private async openOwnWindow(): Promise<number> {
    const win = await this.api.windows.create({
      url: OWN_WINDOW_URL,
      focused: false,
      state: "normal",
    });
    const tabId = win?.tabs?.[0]?.id;
    if (typeof win?.id !== "number" || typeof tabId !== "number") {
      throw new Error("could not open a ChatGPT window for the gateway");
    }
    await this.writeRecords(crypto.randomUUID(), { windowId: win.id, tabIds: [tabId] });
    await this.keepLoaded(tabId);
    return tabId;
  }

  // Another tab in the own window, in the background. Called only from a
  // choice.
  private async openPoolTab(windowId: number): Promise<number> {
    const tab = await this.api.tabs.create({ windowId, url: OWN_WINDOW_URL, active: false });
    if (typeof tab?.id !== "number") throw new Error("the new tab has no id");
    const tabId = tab.id;
    try {
      await this.addToPool(windowId, tabId);
    } catch (err) {
      // Unrecorded, it would sit in the own window as no one's tab, and the
      // next choice would open another.
      await this.api.tabs.remove?.(tabId).catch(() => undefined);
      throw err;
    }
    await this.keepLoaded(tabId);
    return tabId;
  }

  // Adds `tabId` to both records while they are window `windowId`'s (and
  // carry `token`, when given).
  private async addToPool(windowId: number, tabId: number, token?: string): Promise<void> {
    const add = (ids: number[]): number[] => (ids.includes(tabId) ? ids : [...ids, tabId]);
    await this.withRecords(async () => {
      const mark = await this.markRecord();
      if (token !== undefined && (mark === null || mark.token !== token)) return;
      if (mark !== null && mark.windowId === windowId) {
        await this.api.storage.local.set({ [OWN_MARK_KEY]: { ...mark, tabIds: add(mark.tabIds) } });
      }
      const pool = await this.sessionRecord();
      if (pool !== null && pool.windowId === windowId) {
        await this.api.storage.session.set({ [OWN_TAB_KEY]: { windowId, tabIds: add(pool.tabIds) } });
      }
    });
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
    // Every tab of the own window is excluded from every step (a pool tab
    // back from a sign-in page, a tab of it that is not in the pool), and so
    // are the recorded ones while none of them said whether it is an own one.
    const ownWindow = (await this.pool())?.windowId ?? null;
    const usable = (t: chrome.tabs.Tab): t is LiveTab =>
      isChatGptTab(t) && t.windowId !== ownWindow && !this.inDoubt.has(t.id as number);

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

// `{windowId, tabIds}`, or the single-tab `{windowId, tabId}` of an earlier
// version; null when it is neither.
function parsePool(value: unknown): OwnPoolRecord | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as { windowId?: unknown; tabIds?: unknown; tabId?: unknown };
  if (typeof v.windowId !== "number") return null;
  if (Array.isArray(v.tabIds)) {
    const tabIds = v.tabIds.filter((id): id is number => typeof id === "number");
    return { windowId: v.windowId, tabIds: [...new Set(tabIds)] };
  }
  if (typeof v.tabId === "number") return { windowId: v.windowId, tabIds: [v.tabId] };
  return null;
}

// A tab Chrome has not loaded (yet): no page to ask.
function notLoaded(t: chrome.tabs.Tab): boolean {
  return t.discarded === true || t.status === "unloaded";
}

function isMark(found: string | null | undefined): found is string {
  return typeof found === "string";
}

function compareRanks(a: number[], b: number[]): number {
  for (let i = 0; i < a.length; i++) {
    const d = (a[i] as number) - (b[i] as number);
    if (d !== 0) return d;
  }
  return 0;
}

function isChatGptTab(t: chrome.tabs.Tab): boolean {
  return (
    typeof t.id === "number" &&
    typeof t.url === "string" &&
    t.url.startsWith(CHATGPT_URL_PREFIX)
  );
}

// While the first load is in flight only `pendingUrl` is set.
function onChatGptTab(t: chrome.tabs.Tab): boolean {
  const url = t.pendingUrl || t.url;
  return typeof url === "string" && url.startsWith(CHATGPT_URL_PREFIX);
}
